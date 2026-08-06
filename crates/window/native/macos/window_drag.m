// Noticing that the user is dragging a window, on macOS.
//
// The other two platforms are told by the OS: Windows asks `GetGUIThreadInfo`
// whether a window is in a move loop, and GNOME publishes Mutter's grab-op
// signals through the Cursio Shell extension. macOS answers no such question —
// there is no "is this window being moved" API — so the drag has to be
// inferred, and the honest inference is the one the user would make by looking:
// the mouse button is down, and a window has moved since it went down.
//
// ## Why the window list and not the Accessibility API
//
// `AXUIElementCopyElementAtPosition` would name the window under the pointer
// directly, but it needs the Accessibility permission *and* a synchronous
// round trip into the target application, which stalls for as long as that
// application is busy — and an application being dragged is often exactly one
// that is busy. `CGWindowListCopyWindowInfo` reads the window server's own
// snapshot: no IPC with the target, and it needs the Screen Recording
// permission this crate already requires to capture a window's pixels. Asking
// for one permission instead of two matters when the second buys nothing.
//
// ## What counts as a drag
//
// A window whose origin changed while the button has been held. Not "the
// pointer is in a title bar" — that misses every application with a custom
// title bar, which on macOS is most of them — and not "the pointer moved",
// which fires for a text selection. Deliberately also true for a resize, which
// `drag_trigger.rs` does not need to distinguish: a resize does not carry the
// window across a screen edge.

#import <CoreGraphics/CoreGraphics.h>
#import <Foundation/Foundation.h>

#include <string.h>

/// The window latched when the button went down, or 0 when nothing is held.
static CGWindowID g_latched = 0;
/// Where that window was at the moment it was latched, so movement is visible.
static CGPoint g_origin;

/// Copy an NSString into a caller-provided UTF-8 buffer, always terminated.
static void copy_title(NSString *title, char *out, int cap) {
    if (cap <= 0) {
        return;
    }
    out[0] = '\0';
    if (title == nil) {
        return;
    }
    const char *bytes = [title UTF8String];
    if (bytes == NULL) {
        return;
    }
    strncpy(out, bytes, (size_t)cap - 1);
    out[cap - 1] = '\0';
}

/// The frontmost ordinary window containing `point`, and its title and origin.
///
/// "Ordinary" means layer zero: the menu bar, the Dock and every floating panel
/// live above it, and treating one of those as the dragged window would offer
/// the peer something the user cannot drag anywhere.
static BOOL window_at(CGPoint point, CGWindowID *out_id, CGPoint *out_origin,
                      char *title_out, int title_cap) {
    CFArrayRef list = CGWindowListCopyWindowInfo(
        kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements, kCGNullWindowID);
    if (list == NULL) {
        return NO;
    }
    BOOL found = NO;
    // The list is front-to-back, so the first hit is the one the user sees.
    for (NSDictionary *window in (__bridge NSArray *)list) {
        NSNumber *layer = window[(__bridge NSString *)kCGWindowLayer];
        if (layer == nil || [layer intValue] != 0) {
            continue;
        }
        CGRect bounds;
        NSDictionary *rect = window[(__bridge NSString *)kCGWindowBounds];
        if (rect == nil ||
            !CGRectMakeWithDictionaryRepresentation((__bridge CFDictionaryRef)rect, &bounds)) {
            continue;
        }
        if (!CGRectContainsPoint(bounds, point)) {
            continue;
        }
        NSNumber *number = window[(__bridge NSString *)kCGWindowNumber];
        if (number == nil) {
            continue;
        }
        *out_id = (CGWindowID)[number unsignedIntValue];
        *out_origin = bounds.origin;
        // The window's own name where it has one, else the application's. A
        // browser tab has a name; a terminal may leave it empty, and matching
        // "Terminal" still finds it.
        NSString *name = window[(__bridge NSString *)kCGWindowName];
        if (name == nil || [name length] == 0) {
            name = window[(__bridge NSString *)kCGWindowOwnerName];
        }
        copy_title(name, title_out, title_cap);
        found = YES;
        break;
    }
    CFRelease(list);
    return found;
}

/// Where the given window is now, or `NO` if it has gone.
static BOOL origin_of(CGWindowID id, CGPoint *out_origin) {
    CGWindowID ids[1] = {id};
    CFArrayRef targets = CFArrayCreate(NULL, (const void **)ids, 1, NULL);
    if (targets == NULL) {
        return NO;
    }
    CFArrayRef list = CGWindowListCreateDescriptionFromArray(targets);
    CFRelease(targets);
    if (list == NULL) {
        return NO;
    }
    BOOL found = NO;
    if (CFArrayGetCount(list) > 0) {
        NSDictionary *window = ((__bridge NSArray *)list)[0];
        CGRect bounds;
        NSDictionary *rect = window[(__bridge NSString *)kCGWindowBounds];
        if (rect != nil &&
            CGRectMakeWithDictionaryRepresentation((__bridge CFDictionaryRef)rect, &bounds)) {
            *out_origin = bounds.origin;
            found = YES;
        }
    }
    CFRelease(list);
    return found;
}

/// Poll the drag state once.
///
/// Returns 1 while a window is being dragged, 0 otherwise. `title_out` is
/// filled only when the return value is 1. `px`/`py` always receive the current
/// pointer position, in the same global coordinates the window bounds use, so
/// the caller can compare it against a screen edge without conversion.
int cursio_window_drag_poll(char *title_out, int title_cap, double *px, double *py) {
    @autoreleasepool {
        CGEventRef probe = CGEventCreate(NULL);
        CGPoint pointer = probe ? CGEventGetLocation(probe) : CGPointZero;
        if (probe) {
            CFRelease(probe);
        }
        *px = pointer.x;
        *py = pointer.y;

        BOOL held = CGEventSourceButtonState(kCGEventSourceStateCombinedSessionState,
                                             kCGMouseButtonLeft);
        if (!held) {
            g_latched = 0;
            return 0;
        }

        if (g_latched == 0) {
            // Button just went down: remember what is under the pointer and
            // where it sits. Nothing is a drag yet — a click must not be one.
            CGWindowID id = 0;
            CGPoint origin;
            char ignored[1] = {0};
            if (window_at(pointer, &id, &origin, ignored, 1)) {
                g_latched = id;
                g_origin = origin;
            }
            return 0;
        }

        CGPoint now;
        if (!origin_of(g_latched, &now)) {
            // The window closed mid-drag. Forget it rather than reporting a
            // move for something that is no longer there.
            g_latched = 0;
            return 0;
        }
        if (now.x == g_origin.x && now.y == g_origin.y) {
            return 0;
        }

        // It moved: this is a drag. Re-read the title each poll, because a
        // window renames itself freely — a terminal does it on every command —
        // and the name that matters is the one the capture will match against.
        CGWindowID id = 0;
        CGPoint origin;
        if (!window_at(pointer, &id, &origin, title_out, title_cap) || id != g_latched) {
            // The pointer has left the window it is dragging, which happens
            // whenever the drag is faster than the window server redraws. Fall
            // back to the latched window's own description.
            CGWindowID ids[1] = {g_latched};
            CFArrayRef targets = CFArrayCreate(NULL, (const void **)ids, 1, NULL);
            if (targets != NULL) {
                CFArrayRef list = CGWindowListCreateDescriptionFromArray(targets);
                CFRelease(targets);
                if (list != NULL) {
                    if (CFArrayGetCount(list) > 0) {
                        NSDictionary *window = ((__bridge NSArray *)list)[0];
                        NSString *name = window[(__bridge NSString *)kCGWindowName];
                        if (name == nil || [name length] == 0) {
                            name = window[(__bridge NSString *)kCGWindowOwnerName];
                        }
                        copy_title(name, title_out, title_cap);
                    }
                    CFRelease(list);
                }
            }
        }
        return 1;
    }
}
