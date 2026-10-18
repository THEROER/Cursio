/* Places cursio-app's call window where GNOME shows its own banners.
 *
 * The app asks for the bottom-right corner, but a Wayland client cannot place
 * its windows: Mutter ignores the request and puts a new window where its own
 * placement says, which on a fresh desktop is the top-left. A call that turns
 * up in a corner nobody looks at is a call missed, and only the compositor may
 * move a window, so the Shell side does it — top centre of the monitor the
 * pointer is on, where a notification banner would appear, kept above the
 * windows the user is working in.
 *
 * Matched by title rather than application id: the app's other windows (the
 * main one) must keep wherever the user put them. */

import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import St from 'gi://St';

const TITLE = 'Cursio — Call';

/* Clearance from the top of the work area, in logical pixels; about where the
 * Shell's own banners sit below the panel. */
const TOP_MARGIN = 12;

export class CallWindowPlacer {
    constructor() {
        global.display.connectObject(
            'window-created', (_display, window) => this._watch(window),
            this);
    }

    destroy() {
        global.display.disconnectObject(this);
    }

    _watch(window) {
        // The title often arrives after the window is created, so a window is
        // judged once its title is known as well as on creation.
        if (this._place(window))
            return;
        window.connectObject('notify::title', () => {
            if (this._place(window))
                window.disconnectObject(this);
        }, 'unmanaged', () => window.disconnectObject(this), this);
    }

    _place(window) {
        if (window.get_title() !== TITLE)
            return false;
        const actor = window.get_compositor_private();
        // Moved before its first frame, the window still has no size and is
        // placed off centre by exactly half its width.
        if (actor)
            actor.connectObject('first-frame', () => {
                actor.disconnectObject(this);
                this._move(window);
            }, this);
        else
            this._move(window);
        return true;
    }

    _move(window) {
        const monitor = global.display.get_current_monitor();
        const area = Main.layoutManager.getWorkAreaForMonitor(monitor);
        const frame = window.get_frame_rect();
        const scale = St.ThemeContext.get_for_stage(global.stage).scale_factor;
        window.move_frame(true,
            area.x + Math.round((area.width - frame.width) / 2),
            area.y + TOP_MARGIN * scale);
        window.make_above();
    }
}
