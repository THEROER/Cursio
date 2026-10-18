/* The node's control socket, as seen from inside GNOME Shell.
 *
 * Same protocol the app and `cursio status` speak (crates/core/src/control):
 * one JSON object per line each way, requests numbered from 1, and after
 * `subscribe` the node pushes its whole status with id 0 whenever it changes.
 * Subscribing rather than polling is what keeps a battery reading or a peer
 * going offline current without the Shell waking up on a timer.
 *
 * Everything here is asynchronous. This code runs on the compositor's main
 * loop: a blocking connect or read would freeze the whole desktop for as long
 * as the node took to answer, and a node that is busy or wedged is exactly
 * when that would happen.
 *
 * A node that is not running is the ordinary case — the extension loads at
 * login, often before the node — so a failed connect is retried quietly with
 * a growing delay instead of being reported.
 */

import GLib from 'gi://GLib';
import Gio from 'gi://Gio';

const RETRY_MIN_S = 2;
const RETRY_MAX_S = 30;

function socketPath() {
    return GLib.build_filenamev([GLib.get_user_runtime_dir(), 'cursio', 'control.sock']);
}

export class ControlClient {
    /* `onStatus(status | null)` gets every pushed status, and null when the
     * node goes away. */
    constructor(onStatus) {
        this._onStatus = onStatus;
        this._cancellable = null;
        this._conn = null;
        this._input = null;
        this._output = null;
        this._writes = [];
        this._writing = false;
        this._nextId = 1;
        this._pending = new Map();
        this._retryId = 0;
        this._retryS = RETRY_MIN_S;
        this._stopped = false;
    }

    start() {
        this._stopped = false;
        this._connect();
    }

    stop() {
        this._stopped = true;
        if (this._retryId) {
            GLib.source_remove(this._retryId);
            this._retryId = 0;
        }
        this._drop();
    }

    get connected() {
        return this._conn !== null;
    }

    /* Send `command` (an object with `cmd`) and resolve with the reply object.
     * Rejects with the node's own message when it answers with an error. */
    request(command) {
        if (!this._conn)
            return Promise.reject(new Error('Cursio is not running'));
        const id = this._nextId++;
        return new Promise((resolve, reject) => {
            this._pending.set(id, {resolve, reject});
            this._write(`${JSON.stringify({id, ...command})}\n`);
        });
    }

    _connect() {
        if (this._stopped)
            return;
        this._cancellable = new Gio.Cancellable();
        const client = new Gio.SocketClient();
        const address = Gio.UnixSocketAddress.new(socketPath());
        client.connect_async(address, this._cancellable, (c, res) => {
            let conn;
            try {
                conn = c.connect_finish(res);
            } catch (_e) {
                this._scheduleRetry();
                return;
            }
            this._conn = conn;
            this._retryS = RETRY_MIN_S;
            this._input = new Gio.DataInputStream({
                base_stream: conn.get_input_stream(),
                close_base_stream: false,
            });
            this._output = conn.get_output_stream();
            this._readLine();
            this.request({cmd: 'subscribe'}).catch(() => {});
        });
    }

    _scheduleRetry() {
        this._drop();
        if (this._stopped || this._retryId)
            return;
        const delay = this._retryS;
        this._retryS = Math.min(this._retryS * 2, RETRY_MAX_S);
        this._retryId = GLib.timeout_add_seconds(GLib.PRIORITY_LOW, delay, () => {
            this._retryId = 0;
            this._connect();
            return GLib.SOURCE_REMOVE;
        });
    }

    _drop() {
        const wasConnected = this._conn !== null;
        this._cancellable?.cancel();
        this._cancellable = null;
        try {
            this._conn?.close(null);
        } catch (_e) {
            // Already closed by the node; nothing left to release.
        }
        this._conn = null;
        this._input = null;
        this._output = null;
        this._writes = [];
        this._writing = false;
        for (const {reject} of this._pending.values())
            reject(new Error('Cursio stopped'));
        this._pending.clear();
        if (wasConnected)
            this._onStatus(null);
    }

    _readLine() {
        const input = this._input;
        input.read_line_async(GLib.PRIORITY_DEFAULT, this._cancellable, (s, res) => {
            let line;
            try {
                [line] = s.read_line_finish_utf8(res);
            } catch (_e) {
                if (this._input === input)
                    this._scheduleRetry();
                return;
            }
            if (line === null) {
                this._scheduleRetry();
                return;
            }
            this._dispatch(line);
            if (this._input === input)
                this._readLine();
        });
    }

    _dispatch(line) {
        let msg;
        try {
            msg = JSON.parse(line);
        } catch (e) {
            logError(e, 'cursio: unreadable control line');
            return;
        }
        if (msg.id === 0 && msg.reply === 'status') {
            this._onStatus(msg);
            return;
        }
        // A request the node could not parse — a command this node is too old
        // to know — comes back with id 0, since its id was never read. It
        // belongs to the oldest request still waiting.
        let id = msg.id;
        if (id === 0 && this._pending.size > 0)
            id = Math.min(...this._pending.keys());
        const waiter = this._pending.get(id);
        if (!waiter)
            return;
        this._pending.delete(id);
        if (msg.reply === 'error')
            waiter.reject(new Error(msg.message));
        else
            waiter.resolve(msg);
    }

    /* One write at a time: a GOutputStream refuses a second async operation
     * while one is outstanding, and two clicks in quick succession would
     * otherwise lose the second request. */
    _write(text) {
        this._writes.push(new TextEncoder().encode(text));
        this._flush();
    }

    _flush() {
        if (this._writing || this._writes.length === 0 || !this._output)
            return;
        this._writing = true;
        const bytes = this._writes.shift();
        const output = this._output;
        output.write_all_async(bytes, GLib.PRIORITY_DEFAULT, this._cancellable, (s, res) => {
            try {
                s.write_all_finish(res);
            } catch (_e) {
                if (this._output === output)
                    this._scheduleRetry();
                return;
            }
            this._writing = false;
            this._flush();
        });
    }
}
