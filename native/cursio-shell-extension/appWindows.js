/* An application's window, read and driven for the Cursio node: the GNOME
 * half of `cursio window` (the Windows half is `PrintWindow`/`PostMessage`
 * in the node itself).
 *
 * ## Why here and not through a portal
 *
 * On Wayland a client cannot see another client's window. The ScreenCast
 * portal can stream one, but only one the person picks in its dialog, and
 * nothing in a portal stream says which process it belongs to — so "the
 * window of pid 1234" has no portal answer at all. Inside the compositor the
 * question is trivial: `Meta.Window.get_pid()`. The still is the Shell's own
 * screenshot of the window's area and input is a Clutter virtual device, the
 * mechanism the on-screen keyboard uses — neither needs a dialog, and both
 * work the same in an X11 GNOME session.
 *
 * ## Who may ask
 *
 * The session bus is open to every program of this user, and Wayland's whole
 * point is that one of them cannot read or drive another's window. So every
 * method here answers only a caller whose executable is named `cursio` — the
 * node, which asks only after its own per-application consent on this machine
 * (`[apps.<id>] actions`). Anything else is refused.
 *
 * The window is raised and focused before it is read or driven: the still is
 * what is on screen in its frame, and keys go to the focused window. The
 * pointer is put back where it was after a click.
 */

import Clutter from 'gi://Clutter';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import Meta from 'gi://Meta';
import Shell from 'gi://Shell';

Gio._promisify(Shell.Screenshot.prototype, 'screenshot_area');

/* Time for a just-raised window to be drawn on top before it is read. */
const RAISE_SETTLE_MS = 150;

/* The keysym for Return, which a newline in typed text stands for. */
const KEY_RETURN = 0xff0d;

export const APP_WINDOWS_IFACE = `
    <method name="AppWindows">
      <arg type="au" name="pids" direction="in"/>
      <arg type="a(usuu)" name="windows" direction="out"/>
    </method>
    <method name="ShotAppWindow">
      <arg type="u" name="id" direction="in"/>
      <arg type="ay" name="png" direction="out"/>
    </method>
    <method name="ClickAppWindow">
      <arg type="u" name="id" direction="in"/>
      <arg type="d" name="x" direction="in"/>
      <arg type="d" name="y" direction="in"/>
      <arg type="b" name="right" direction="in"/>
    </method>
    <method name="TypeAppWindow">
      <arg type="u" name="id" direction="in"/>
      <arg type="s" name="text" direction="in"/>
    </method>
    <method name="KeyAppWindow">
      <arg type="u" name="id" direction="in"/>
      <arg type="u" name="evdev" direction="in"/>
    </method>`;

/* Whether the D-Bus caller is the Cursio node. */
function callerIsCursio(invocation) {
    try {
        const reply = Gio.DBus.session.call_sync(
            'org.freedesktop.DBus', '/org/freedesktop/DBus', 'org.freedesktop.DBus',
            'GetConnectionUnixProcessID',
            new GLib.Variant('(s)', [invocation.get_sender()]),
            null, Gio.DBusCallFlags.NONE, -1, null);
        const [pid] = reply.deepUnpack();
        const exe = GLib.file_read_link(`/proc/${pid}/exe`);
        return GLib.path_get_basename(exe) === 'cursio';
    } catch (e) {
        logError(e, 'cursio: cannot tell who asked for a window');
        return false;
    }
}

function refuse(invocation, message) {
    invocation.return_dbus_error('dev.cursio.Shell.Error.Refused', message);
}

function now() {
    return GLib.get_monotonic_time();
}

function settle(ms) {
    return new Promise(resolve => GLib.timeout_add(GLib.PRIORITY_DEFAULT, ms, () => {
        resolve();
        return GLib.SOURCE_REMOVE;
    }));
}

/* An application window: what a person would call one, not a tooltip or a
 * menu of it. */
function isAppWindow(win) {
    const type = win.get_window_type();
    return !win.is_override_redirect() &&
        (type === Meta.WindowType.NORMAL || type === Meta.WindowType.DIALOG);
}

export class AppWindows {
    constructor() {
        this._pointer = null;
        this._keyboard = null;
    }

    destroy() {
        this._pointer?.run_dispose?.();
        this._keyboard?.run_dispose?.();
        this._pointer = null;
        this._keyboard = null;
    }

    _windows() {
        return global.get_window_actors()
            .map(actor => actor.get_meta_window())
            .filter(win => win && isAppWindow(win));
    }

    _byId(id) {
        return this._windows().find(win => win.get_stable_sequence() === id) ?? null;
    }

    async _raise(win) {
        if (win.minimized)
            win.unminimize();
        win.activate(global.get_current_time());
        await settle(RAISE_SETTLE_MS);
    }

    _seat() {
        return Clutter.get_default_backend().get_default_seat();
    }

    AppWindowsAsync(params, invocation) {
        if (!callerIsCursio(invocation)) {
            refuse(invocation, 'only the Cursio node may list application windows');
            return;
        }
        const [pids] = params;
        const wanted = new Set(pids);
        const found = this._windows()
            .filter(win => wanted.has(win.get_pid()))
            .map(win => {
                const rect = win.get_frame_rect();
                return [win.get_stable_sequence(), win.get_title() ?? '', rect.width, rect.height];
            });
        invocation.return_value(new GLib.Variant('(a(usuu))', [found]));
    }

    async ShotAppWindowAsync(params, invocation) {
        if (!callerIsCursio(invocation)) {
            refuse(invocation, 'only the Cursio node may read an application window');
            return;
        }
        const [id] = params;
        const win = this._byId(id);
        if (!win) {
            refuse(invocation, 'that window is gone');
            return;
        }
        try {
            await this._raise(win);
            const rect = win.get_frame_rect();
            const stream = Gio.MemoryOutputStream.new_resizable();
            const shooter = new Shell.Screenshot();
            await shooter.screenshot_area(rect.x, rect.y, rect.width, rect.height, stream);
            stream.close(null);
            const png = stream.steal_as_bytes().get_data();
            invocation.return_value(new GLib.Variant('(ay)', [png]));
        } catch (e) {
            logError(e, 'cursio: reading an application window');
            refuse(invocation, `the Shell could not read the window: ${e.message}`);
        }
    }

    async ClickAppWindowAsync(params, invocation) {
        if (!callerIsCursio(invocation)) {
            refuse(invocation, 'only the Cursio node may click an application window');
            return;
        }
        const [id, fx, fy, right] = params;
        const win = this._byId(id);
        if (!win) {
            refuse(invocation, 'that window is gone');
            return;
        }
        await this._raise(win);
        const rect = win.get_frame_rect();
        this._pointer ??= this._seat().create_virtual_device(Clutter.InputDeviceType.POINTER_DEVICE);
        const [backX, backY] = global.get_pointer();
        const button = right ? Clutter.BUTTON_SECONDARY : Clutter.BUTTON_PRIMARY;
        this._pointer.notify_absolute_motion(now(), rect.x + fx * rect.width, rect.y + fy * rect.height);
        this._pointer.notify_button(now(), button, Clutter.ButtonState.PRESSED);
        this._pointer.notify_button(now(), button, Clutter.ButtonState.RELEASED);
        this._pointer.notify_absolute_motion(now(), backX, backY);
        invocation.return_value(null);
    }

    async TypeAppWindowAsync(params, invocation) {
        if (!callerIsCursio(invocation)) {
            refuse(invocation, 'only the Cursio node may type into an application window');
            return;
        }
        const [id, text] = params;
        const win = this._byId(id);
        if (!win) {
            refuse(invocation, 'that window is gone');
            return;
        }
        await this._raise(win);
        this._keyboard ??= this._seat().create_virtual_device(Clutter.InputDeviceType.KEYBOARD_DEVICE);
        // Keysyms rather than key codes: Mutter maps one missing from the
        // layout onto a spare code for the moment, so any script types.
        for (const ch of text) {
            const keyval = ch === '\n' ? KEY_RETURN : Clutter.unicode_to_keysym(ch.codePointAt(0));
            this._keyboard.notify_keyval(now(), keyval, Clutter.KeyState.PRESSED);
            this._keyboard.notify_keyval(now(), keyval, Clutter.KeyState.RELEASED);
        }
        invocation.return_value(null);
    }

    async KeyAppWindowAsync(params, invocation) {
        if (!callerIsCursio(invocation)) {
            refuse(invocation, 'only the Cursio node may press keys in an application window');
            return;
        }
        const [id, evdev] = params;
        const win = this._byId(id);
        if (!win) {
            refuse(invocation, 'that window is gone');
            return;
        }
        await this._raise(win);
        this._keyboard ??= this._seat().create_virtual_device(Clutter.InputDeviceType.KEYBOARD_DEVICE);
        this._keyboard.notify_key(now(), evdev, Clutter.KeyState.PRESSED);
        this._keyboard.notify_key(now(), evdev, Clutter.KeyState.RELEASED);
        invocation.return_value(null);
    }
}
