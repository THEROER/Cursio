/* Cursio GNOME Shell extension — the window-drag bridge.
 *
 * ## Why this exists
 *
 * Cursio's window transfer is meant to be started by *dragging* a window past
 * the screen edge toward the paired machine. On Windows the daemon can detect
 * that by itself (`GetGUIThreadInfo` reports `GUI_INMOVESIZE`). On Wayland it
 * cannot: a client may not know that other windows exist, let alone that the
 * user is moving one. Every route was checked on GNOME 49 and is closed —
 * `Shell.Eval` is dev-mode only, `Shell.Introspect.GetWindows` returns
 * `AccessDenied`, and Mutter does not implement `ext_foreign_toplevel_list`.
 *
 * A Shell extension runs *inside* the compositor, where the information is
 * simply available. So this is a deliberately thin bridge: it watches Mutter's
 * own grab-op signals and re-publishes them on the session bus. It makes no
 * decisions — whether a drag means a transfer is the daemon's business, and
 * keeping the policy there is what stops this file from growing into a second
 * implementation of Cursio.
 *
 * ## Contract
 *
 * Bus name `dev.cursio.Shell`, object `/dev/cursio/Shell`:
 *   WindowDragStarted(s title, i x, i y)  — a move grab began
 *   WindowDragMoved(i x, i y)             — pointer moved during that grab
 *   WindowDragEnded(i x, i y)             — the grab finished
 *   GetDraggedWindow() -> (s, i, i, i, i) — title + frame rect, or ("",0,0,0,0)
 *
 * Coordinates are the pointer's, in compositor-global pixels, which is what
 * the daemon's edge logic already speaks.
 *
 * The daemon must tolerate this extension being absent, disabled, or a version
 * behind: it is an optional accelerator for a feature that also works from the
 * command line, never a dependency.
 */

import GLib from 'gi://GLib';
import Gio from 'gi://Gio';
import Meta from 'gi://Meta';

const IFACE = `
<node>
  <interface name="dev.cursio.Shell">
    <signal name="WindowDragStarted">
      <arg type="s" name="title"/>
      <arg type="i" name="x"/>
      <arg type="i" name="y"/>
    </signal>
    <signal name="WindowDragMoved">
      <arg type="i" name="x"/>
      <arg type="i" name="y"/>
    </signal>
    <signal name="WindowDragEnded">
      <arg type="i" name="x"/>
      <arg type="i" name="y"/>
    </signal>
    <method name="GetDraggedWindow">
      <arg type="s" name="title" direction="out"/>
      <arg type="i" name="x" direction="out"/>
      <arg type="i" name="y" direction="out"/>
      <arg type="i" name="width" direction="out"/>
      <arg type="i" name="height" direction="out"/>
    </method>
  </interface>
</node>`;

/* How often the pointer is sampled during a drag, in milliseconds.
 *
 * Mutter has no "pointer moved during grab" signal, so this polls. 30ms (~33Hz)
 * is well under the time it takes to shove a window at an edge, and the timer
 * only exists while a drag is actually in progress — there is no idle cost. */
const POLL_MS = 30;

/* Mutter ORs this flag into the grab op when the move is unconstrained.
 * Comparing without masking it off silently fails to recognise a plain move;
 * GNOME's own tiling-assistant does exactly this. */
const GRAB_OP_WINDOW_FLAG_UNCONSTRAINED = 1024;

export default class CursioExtension {
    enable() {
        this._dbus = Gio.DBusExportedObject.wrapJSObject(IFACE, this);
        this._dbus.export(Gio.DBus.session, '/dev/cursio/Shell');

        // Claim the name only after the object is exported, so a client that
        // reacts to the name appearing never finds an empty path.
        this._nameId = Gio.bus_own_name(
            Gio.BusType.SESSION,
            'dev.cursio.Shell',
            Gio.BusNameOwnerFlags.NONE,
            null, null, null);

        this._window = null;
        this._pollId = 0;

        global.display.connectObject(
            'grab-op-begin', this._onGrabBegin.bind(this),
            'grab-op-end', this._onGrabEnd.bind(this),
            this);
    }

    disable() {
        // Must fully unwind: an extension is disabled on screen lock, and
        // anything left behind (a timer, a signal, a bus name) leaks for the
        // rest of the session and is re-added on unlock.
        this._stopPolling();
        global.display.disconnectObject(this);

        if (this._nameId) {
            Gio.bus_unown_name(this._nameId);
            this._nameId = 0;
        }
        if (this._dbus) {
            this._dbus.unexport();
            this._dbus = null;
        }
        this._window = null;
    }

    /* --- Mutter signals --------------------------------------------------- */

    _onGrabBegin(_display, window, grabOp) {
        if (!window || !this._isMove(grabOp))
            return;

        this._window = window;
        const [x, y] = this._pointer();
        this._emit('WindowDragStarted',
            new GLib.Variant('(sii)', [window.get_title() ?? '', x, y]));
        this._startPolling();
    }

    _onGrabEnd(_display, _window, _grabOp) {
        if (!this._window)
            return;

        this._stopPolling();
        this._window = null;
        const [x, y] = this._pointer();
        this._emit('WindowDragEnded', new GLib.Variant('(ii)', [x, y]));
    }

    /* Whether this grab is a *move*. Resizes are deliberately excluded: they
     * change a window's size in place and are never an attempt to send it
     * somewhere. */
    _isMove(grabOp) {
        const op = grabOp & ~GRAB_OP_WINDOW_FLAG_UNCONSTRAINED;
        return op === Meta.GrabOp.MOVING || op === Meta.GrabOp.KEYBOARD_MOVING;
    }

    /* --- Pointer polling -------------------------------------------------- */

    _startPolling() {
        this._stopPolling();
        this._pollId = GLib.timeout_add(GLib.PRIORITY_DEFAULT, POLL_MS, () => {
            // The grab can end without `grab-op-end` reaching us (the window
            // closed mid-drag). Stop on our own rather than polling forever.
            if (!this._window) {
                this._pollId = 0;
                return GLib.SOURCE_REMOVE;
            }
            const [x, y] = this._pointer();
            this._emit('WindowDragMoved', new GLib.Variant('(ii)', [x, y]));
            return GLib.SOURCE_CONTINUE;
        });
    }

    _stopPolling() {
        if (this._pollId) {
            GLib.source_remove(this._pollId);
            this._pollId = 0;
        }
    }

    /* --- D-Bus ------------------------------------------------------------ */

    GetDraggedWindow() {
        if (!this._window)
            return ['', 0, 0, 0, 0];

        const r = this._window.get_frame_rect();
        return [this._window.get_title() ?? '', r.x, r.y, r.width, r.height];
    }

    _pointer() {
        const [x, y] = global.get_pointer();
        return [x, y];
    }

    /* Emitting must never throw into Mutter's signal handler: a client that
     * disconnected mid-drag would otherwise take out the compositor's grab
     * handling with it. */
    _emit(name, params) {
        try {
            this._dbus?.emit_signal(name, params);
        } catch (e) {
            logError(e, `cursio: failed to emit ${name}`);
        }
    }
}
