//! A throwaway uinput mouse for `live-test`'s synthetic pointer motion.
//!
//! It enters the same path a hand on the real mouse does — kernel, compositor,
//! the InputCapture portal, Cursio's capture — so the latency it produces is
//! the one the user feels, not a shortcut around capture. That is also why it
//! must never run unannounced: while the cursor is on this machine it moves
//! the user's own pointer. It traces small closed circles, so wherever the
//! pointer is, it ends where it started.
//!
//! Hand-rolled over `libc` rather than the `evdev` crate: xtask stays free of
//! the app crates' dependency tree, and five ioctls are all it needs.
//!
//! The name must not start with "Cursio": the evdev capture path skips
//! devices named that way so the daemon never re-captures its own injection.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

const NAME: &[u8] = b"xtask live-test pointer";

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const SYN_REPORT: u16 = 0;
const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
/// Without a button the compositor does not classify the device as a mouse.
const BTN_LEFT: u16 = 0x110;
const BUS_VIRTUAL: u16 = 0x06;

// _IO / _IOW('U', n, ...) from linux/uinput.h.
const UI_DEV_CREATE: libc::c_ulong = 0x5501;
const UI_DEV_DESTROY: libc::c_ulong = 0x5502;
const UI_DEV_SETUP: libc::c_ulong = 0x405c_5503;
const UI_SET_EVBIT: libc::c_ulong = 0x4004_5564;
const UI_SET_KEYBIT: libc::c_ulong = 0x4004_5565;
const UI_SET_RELBIT: libc::c_ulong = 0x4004_5566;

#[repr(C)]
struct InputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

#[repr(C)]
struct UinputSetup {
    id: InputId,
    name: [u8; 80],
    ff_effects_max: u32,
}

#[repr(C)]
struct InputEvent {
    time: libc::timeval,
    kind: u16,
    code: u16,
    value: i32,
}

pub struct Pointer {
    file: File,
}

impl Pointer {
    pub fn create() -> Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open("/dev/uinput")
            .context("opening /dev/uinput (this user needs write access, as the daemon does)")?;
        let fd = file.as_raw_fd();
        let mut setup = UinputSetup {
            id: InputId {
                bustype: BUS_VIRTUAL,
                vendor: 0x1d6b,
                product: 0x7e57,
                version: 1,
            },
            name: [0; 80],
            ff_effects_max: 0,
        };
        setup.name[..NAME.len()].copy_from_slice(NAME);
        // SAFETY: fd is open for the duration; each request takes an int or a
        // pointer to the matching repr(C) struct, as linux/uinput.h defines.
        unsafe {
            for (req, arg) in [
                (UI_SET_EVBIT, EV_KEY),
                (UI_SET_KEYBIT, BTN_LEFT),
                (UI_SET_EVBIT, EV_REL),
                (UI_SET_RELBIT, REL_X),
                (UI_SET_RELBIT, REL_Y),
            ] {
                if libc::ioctl(fd, req, libc::c_int::from(arg)) < 0 {
                    bail!(
                        "uinput setup ioctl failed: {}",
                        std::io::Error::last_os_error()
                    );
                }
            }
            if libc::ioctl(fd, UI_DEV_SETUP, &setup as *const UinputSetup) < 0
                || libc::ioctl(fd, UI_DEV_CREATE) < 0
            {
                bail!(
                    "creating the uinput device failed: {}",
                    std::io::Error::last_os_error()
                );
            }
        }
        // The compositor needs a moment to open a new device; motion before
        // that is lost.
        std::thread::sleep(Duration::from_millis(800));
        Ok(Self { file })
    }

    fn emit(&mut self, kind: u16, code: u16, value: i32) -> std::io::Result<()> {
        let ev = InputEvent {
            time: libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
            kind,
            code,
            value,
        };
        // SAFETY: InputEvent is repr(C) plain data; the slice covers exactly it.
        let bytes = unsafe {
            std::slice::from_raw_parts(
                (&ev as *const InputEvent).cast::<u8>(),
                std::mem::size_of::<InputEvent>(),
            )
        };
        self.file.write_all(bytes)
    }

    pub fn nudge(&mut self, dx: i32, dy: i32) -> std::io::Result<()> {
        self.emit(EV_REL, REL_X, dx)?;
        self.emit(EV_REL, REL_Y, dy)?;
        self.emit(EV_SYN, SYN_REPORT, 0)
    }

    /// Trace closed circles at `hz` until `until`, returning the event count.
    pub fn circles(&mut self, until: Instant, hz: u32) -> Result<u64> {
        let steps = circle_steps(24, 6.0);
        let period = Duration::from_secs(1) / hz;
        let mut sent = 0;
        let mut next = Instant::now();
        'outer: loop {
            for &(dx, dy) in &steps {
                if Instant::now() >= until {
                    break 'outer;
                }
                self.nudge(dx, dy).context("writing to the uinput device")?;
                sent += 1;
                next += period;
                if let Some(wait) = next.checked_duration_since(Instant::now()) {
                    std::thread::sleep(wait);
                }
            }
        }
        Ok(sent)
    }
}

impl Drop for Pointer {
    fn drop(&mut self) {
        // SAFETY: fd still open; destroying an already-gone device only errors.
        unsafe {
            libc::ioctl(self.file.as_raw_fd(), UI_DEV_DESTROY);
        }
    }
}

/// Integer deltas around a circle of `radius` in `n` steps that sum to zero,
/// so a full lap returns the pointer exactly to where it began.
pub fn circle_steps(n: usize, radius: f64) -> Vec<(i32, i32)> {
    let point = |i: usize| {
        let a = std::f64::consts::TAU * i as f64 / n as f64;
        (
            (radius * a.cos()).round() as i32,
            (radius * a.sin()).round() as i32,
        )
    };
    (0..n)
        .map(|i| {
            let (x0, y0) = point(i);
            let (x1, y1) = point(i + 1);
            (x1 - x0, y1 - y0)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lap_returns_the_pointer_to_where_it_started() {
        let steps = circle_steps(24, 6.0);
        let (x, y) = steps
            .iter()
            .fold((0, 0), |(x, y), (dx, dy)| (x + dx, y + dy));
        assert_eq!((x, y), (0, 0));
        assert!(steps.iter().any(|s| *s != (0, 0)));
    }

    #[test]
    fn structs_match_the_kernel_abi() {
        assert_eq!(std::mem::size_of::<UinputSetup>(), 92);
        assert_eq!(std::mem::size_of::<InputEvent>(), 24);
    }
}
