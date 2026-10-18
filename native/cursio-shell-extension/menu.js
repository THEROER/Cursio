/* The Cursio tile in GNOME's quick settings: every machine in the mesh, its
 * battery and network, and what can be done with it from here.
 *
 * It renders what the node reports and holds no state of its own, the same
 * split as the app: the node is the only thing that knows the mesh, and a
 * second copy of that knowledge here would drift from it. Every action is a
 * control-socket command the node carries out.
 *
 * The menu is rebuilt only when something it shows has changed. The node
 * pushes its status on every change — latency readings included — and
 * rebuilding an open menu on each push would close a submenu under the
 * user's pointer.
 */

import Clutter from 'gi://Clutter';
import GLib from 'gi://GLib';
import GObject from 'gi://GObject';
import Gio from 'gi://Gio';
import St from 'gi://St';

import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import * as PopupMenu from 'resource:///org/gnome/shell/ui/popupMenu.js';
import * as QuickSettings from 'resource:///org/gnome/shell/ui/quickSettings.js';

import {ControlClient} from './control.js';

const TILE_ICON = 'video-joined-displays-symbolic';

function kindIcon(kind) {
    switch (kind) {
    case 'Phone':
        return 'phone-symbolic';
    case 'Tablet':
        return 'tablet-symbolic';
    case 'Laptop':
        return 'computer-laptop-symbolic';
    default:
        return 'computer-symbolic';
    }
}

/* `None` from the node means "cannot say", so it yields no icon at all —
 * never an empty battery or no signal. */
function batteryIcon(status) {
    if (status?.battery_percent == null)
        return null;
    const level = Math.min(100, Math.round(status.battery_percent / 10) * 10);
    const charging = status.power === 'Charging' ? '-charging' : '';
    return `battery-level-${level}${charging}-symbolic`;
}

function networkIcon(status) {
    switch (status?.network) {
    case 'Wired':
        return 'network-wired-symbolic';
    case 'Cellular':
        return 'network-cellular-signal-good-symbolic';
    case 'Wifi': {
        const dbm = status.signal_dbm;
        if (dbm == null)
            return 'network-wireless-symbolic';
        if (dbm >= -55)
            return 'network-wireless-signal-excellent-symbolic';
        if (dbm >= -67)
            return 'network-wireless-signal-good-symbolic';
        if (dbm >= -75)
            return 'network-wireless-signal-ok-symbolic';
        return 'network-wireless-signal-weak-symbolic';
    }
    default:
        return null;
    }
}

function mountPath(peer) {
    return GLib.build_filenamev([GLib.get_home_dir(), 'Cursio', peer]);
}

/* The machine's header row: name on the left, its state as icons on the
 * right, the way the system's own device rows look. */
const PeerHeader = GObject.registerClass(
class PeerHeader extends PopupMenu.PopupBaseMenuItem {
    _init(peer) {
        super._init({reactive: false, can_focus: false, style_class: 'popup-menu-item'});
        const link = peer.link;
        const online = peer.presence === 'online' && link;

        this.add_child(new St.Icon({
            icon_name: kindIcon(link?.device_kind),
            style_class: 'popup-menu-icon',
        }));
        this.add_child(new St.Label({
            text: peer.name,
            y_align: Clutter.ActorAlign.CENTER,
            style: 'font-weight: bold;',
            x_expand: true,
        }));
        if (!online) {
            this.add_child(new St.Label({text: 'offline', y_align: Clutter.ActorAlign.CENTER}));
            this.opacity = 128;
            return;
        }
        const net = networkIcon(link.status);
        if (net)
            this.add_child(new St.Icon({icon_name: net, style_class: 'popup-menu-icon'}));
        const battery = batteryIcon(link.status);
        if (battery) {
            this.add_child(new St.Label({
                text: `${link.status.battery_percent}%`,
                y_align: Clutter.ActorAlign.CENTER,
            }));
            this.add_child(new St.Icon({icon_name: battery, style_class: 'popup-menu-icon'}));
        }
    }
});

const CursioToggle = GObject.registerClass(
class CursioToggle extends QuickSettings.QuickMenuToggle {
    _init() {
        super._init({
            title: 'Cursio',
            subtitle: 'not running',
            iconName: TILE_ICON,
            toggleMode: false,
        });
        this.menu.setHeader(TILE_ICON, 'Cursio', 'Your machines, together');
        this._peers = new PopupMenu.PopupMenuSection();
        this._problems = new PopupMenu.PopupMenuSection();
        this.menu.addMenuItem(this._peers);
        this.menu.addMenuItem(this._problems);
        this.menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());
        this.menu.addAction('Cursio Settings', () => this._openSettings());

        this._status = null;
        this._shown = null;
        this._client = new ControlClient(status => this._onStatus(status));
        // The tile opens its menu, as GSConnect's does: there is nothing to
        // switch on or off from here, the node runs as a service.
        this.connect('clicked', () => this.menu.open());
        this._client.start();
        this._render();
    }

    destroy() {
        this._client.stop();
        super.destroy();
    }

    _onStatus(status) {
        this._status = status;
        this._render();
    }

    /* What the menu shows, reduced to the fields it reads. Equal keys mean
     * nothing visible changed and the open menu is left alone. */
    _key(status) {
        if (!status)
            return 'down';
        return JSON.stringify({
            peers: status.peers.map(p => [p.name, p.presence, p.link?.device_kind,
                p.link?.status, p.link?.modules]),
            problems: status.problems.map(p => [p.severity, p.summary]),
            muted: this._mutedPeers(status),
        });
    }

    _mutedPeers(status) {
        const names = new Set(status.streams.map(s => s.peer));
        return [...names].filter(n => this._peerMuted(status, n)).sort();
    }

    _peerMuted(status, name) {
        const streams = status.streams.filter(s => s.peer === name);
        return streams.length > 0 && streams.every(s => s.muted);
    }

    _render() {
        const status = this._status;
        this.checked = status !== null;
        const online = status ? status.peers.filter(p => p.presence === 'online') : [];
        if (!status)
            this.subtitle = 'not running';
        else if (online.length === 1)
            this.subtitle = online[0].name;
        else if (online.length === 0)
            this.subtitle = 'no machines online';
        else
            this.subtitle = `${online.length} machines online`;

        const key = this._key(status);
        if (key === this._shown)
            return;
        this._shown = key;

        this._peers.removeAll();
        this._problems.removeAll();
        if (!status) {
            const item = new PopupMenu.PopupMenuItem('Cursio is not running', {reactive: false});
            this._peers.addMenuItem(item);
            return;
        }
        const ordered = [...status.peers].sort((a, b) =>
            (a.presence === 'online' ? 0 : 1) - (b.presence === 'online' ? 0 : 1));
        for (const peer of ordered)
            this._addPeer(status, peer);

        const serious = status.problems.filter(p => p.severity !== 'Info');
        if (serious.length > 0) {
            this._problems.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());
            for (const problem of serious.slice(0, 4)) {
                const item = new PopupMenu.PopupImageMenuItem(problem.summary,
                    'dialog-warning-symbolic', {reactive: false});
                item.label.clutter_text.line_wrap = true;
                this._problems.addMenuItem(item);
            }
        }
    }

    _addPeer(status, peer) {
        this._peers.addMenuItem(new PeerHeader(peer));
        const link = peer.presence === 'online' ? peer.link : null;
        if (!link)
            return;
        const has = module => link.modules.includes(module);
        const name = peer.name;

        if (link.device_kind === 'Phone')
            this._peers.addAction('Ring', () => this._run({cmd: 'ring', peer: name}, name));
        if (has('share')) {
            this._peers.addAction('Send Files…', () => this._sendFiles(name));
            this._peers.addAction('Send Clipboard', () => this._sendClipboard(name));
        }
        if (has('files') && GLib.file_test(mountPath(name), GLib.FileTest.IS_DIR))
            this._peers.addAction('Browse Files', () => this._browse(name));
        if (status.streams.some(s => s.peer === name)) {
            const sound = new PopupMenu.PopupSwitchMenuItem('Sound',
                !this._peerMuted(status, name));
            sound.connect('toggled', (_item, on) => this._setMuted(name, !on));
            this._peers.addMenuItem(sound);
        }
    }

    async _run(command, peer) {
        try {
            await this._client.request(command);
        } catch (e) {
            // An older node cannot parse a command it predates, and says so
            // by listing every command it does know.
            const message = e.message.includes('unknown variant')
                ? 'the running Cursio is too old for this; update it'
                : e.message;
            Main.notify(`Cursio — ${peer}`, message);
        }
    }

    _sendFiles(peer) {
        this.menu.close();
        // A Shell extension cannot open a GTK file chooser in its own process;
        // zenity is the chooser the desktop already ships.
        let proc;
        try {
            proc = Gio.Subprocess.new(
                ['zenity', '--file-selection', '--multiple', '--separator=\n',
                    `--title=Send to ${peer}`],
                Gio.SubprocessFlags.STDOUT_PIPE);
        } catch (e) {
            Main.notify('Cursio', `cannot open a file chooser: ${e.message}; install zenity`);
            return;
        }
        proc.communicate_utf8_async(null, null, (p, res) => {
            let out;
            try {
                [, out] = p.communicate_utf8_finish(res);
            } catch (e) {
                logError(e, 'cursio: file chooser');
                return;
            }
            if (!p.get_successful() || !out)
                return;
            for (const path of out.split('\n').filter(l => l.length > 0))
                this._run({cmd: 'share', peer, target: path, action: 'Save'}, peer);
        });
    }

    _sendClipboard(peer) {
        St.Clipboard.get_default().get_text(St.ClipboardType.CLIPBOARD, (_c, text) => {
            if (!text) {
                Main.notify('Cursio', 'the clipboard holds no text to send');
                return;
            }
            // A link is opened on the other side; anything else lands on its
            // clipboard, which is what copying it here meant.
            const action = /^\s*https?:\/\/\S+\s*$/.test(text) ? 'Open' : 'Copy';
            this._run({cmd: 'share', peer, target: text, action}, peer);
        });
    }

    _browse(peer) {
        this.menu.close();
        const uri = Gio.File.new_for_path(mountPath(peer)).get_uri();
        Gio.AppInfo.launch_default_for_uri_async(uri, null, null, (_o, res) => {
            try {
                Gio.AppInfo.launch_default_for_uri_finish(res);
            } catch (e) {
                Main.notify(`Cursio — ${peer}`, e.message);
            }
        });
    }

    _setMuted(peer, muted) {
        for (const stream of this._status?.streams ?? []) {
            if (stream.peer !== peer)
                continue;
            this._run({
                cmd: 'set_stream_volume',
                stream: stream.id,
                volume: stream.volume,
                muted,
            }, peer);
        }
    }

    _openSettings() {
        // The root's stable launcher, never a `versions/<v>/` path: it hands
        // over to whichever build is current.
        const app = GLib.build_filenamev([GLib.get_home_dir(), '.local', 'lib', 'cursio',
            'cursio-app']);
        try {
            Gio.Subprocess.new([app], Gio.SubprocessFlags.NONE);
        } catch (e) {
            Main.notify('Cursio', `cannot start the Cursio app: ${e.message}`);
        }
    }
});

export const CursioIndicator = GObject.registerClass(
class CursioIndicator extends QuickSettings.SystemIndicator {
    _init() {
        super._init();
        this._toggle = new CursioToggle();
        this.quickSettingsItems.push(this._toggle);
        Main.panel.statusArea.quickSettings.addExternalIndicator(this);
    }

    destroy() {
        for (const item of this.quickSettingsItems)
            item.destroy();
        this.quickSettingsItems = [];
        super.destroy();
    }
});
