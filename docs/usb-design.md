# USB Design (a device on one machine, usable from any of them)

Status: **designed, not built.** Companion to `files-design.md` (the other
module that carries a kernel's requests over the link) and
`platform-design.md`'s rules for new modules.

## What this is for

A phone is plugged into the laptop; `adb devices` on the desktop should list it.
A programmer, a security key, a printer — whichever machine has the port, every
machine has the device. The USB port stops being a property of one box and
becomes a property of the mesh.

This is deliberately *not* a per-device feature. There is no adb module, no
printer module: the shared thing is the bus, and everything speaking USB rides
it for free.

## What Cursio does not implement

**The URB transport is USB/IP, and Cursio does not reimplement it.**

Linux ships both halves of USB/IP in mainline: `usbip-host` detaches a local
device and exports its transfers, `vhci-hcd` is a virtual host controller that
makes a remote device appear as though it were plugged in locally. Both are
kernel drivers, both are stable, and the protocol between them is fixed. On
Windows the importing half is `usbipd-win`, which ships its own VHCI driver.

Writing our own URB vocabulary would mean writing a kernel driver on both
platforms to consume it — the `native/cursio-vad` road, taken again, to arrive
where mainline already is. Audio needed that because Windows genuinely offers no
way to publish an endpoint from user space. USB does not: the endpoint exists,
it just has no authentication and no discovery.

So the split is:

| Concern | Owner |
|---|---|
| URB framing, device emulation, kernel plumbing | USB/IP (`usbip-host`, `vhci-hcd`, `usbipd-win`) |
| Discovery, trust, encryption, ownership, reconnect | Cursio |

Cursio carries the USB/IP byte stream inside the session it already has, and
adds the parts USB/IP lacks.

## Why carrying it is worth doing

`usbipd` listens on TCP 3240 with **no authentication whatsoever**. Whoever
reaches the port takes the device — it is a remote-attach primitive with the
access control left as an exercise. The standing advice is to never expose it
beyond a trusted link, which is precisely the thing this project already built:
SPAKE2 pairing, a trust store, and a Noise channel between machines that have
confirmed each other.

Carrying USB/IP over the mesh channel means the daemon never binds a public
port. It listens on loopback; the module dials loopback locally and the bytes
cross the link inside Noise, authenticated as the peer they came from.

Discovery comes free for the same reason. `usbip list -r <host>` needs a host;
the mesh already knows every peer and survives their coming and going.

## Ownership: a device lives on one machine at a time

The hard truth this design cannot engineer around: **`usbip bind` takes the
device away from its local kernel.** It rebinds the device from its class driver
to `usbip-host`. While the phone is attached to the desktop, the laptop it is
physically plugged into no longer has it — no MTP mount, no `adb`, nothing.

"Available on every machine simultaneously" is not achievable at the bus layer,
and no amount of protocol design changes it: USB has exactly one host per
device. What is achievable, and what this module targets, is **available on any
machine, one at a time, switched in a second**.

That is the same shape as the shared cursor, and it reuses the same reasoning
(`input-design.md`, and `CursorRole` in the code):

- Exactly one **holder** at a time; every other node sees the device listed but
  not attached.
- **Only the holder may hand it away.** An idle machine claiming a device it
  does not hold is the USB version of the stray-pointer bug that made a machine
  start driving a peer nobody was at.
- The **host** — the machine with the physical port — is the authority on
  whether a device exists at all, and may always revoke.

Rule 4 in `platform-design.md` ("one owner per piece of state") is what this is;
the holder is the owner, and everyone else requests.

## Vocabulary

`ModuleId::USB = ModuleId(7)`, symmetric — every machine both offers its ports
and attaches peers' devices. Two priority classes, deliberately split:

- **Control** (`Priority::Control`) — the device list, claims, releases,
  revocations. Small, rare, and must not queue behind a bulk transfer, or a
  release would be delayed by the very transfer it is trying to end.
- **Data** (`Priority::Bulk`) — the URB stream itself. This is the honest class
  under rule 2: a file copy off a USB drive is bulk traffic and must yield to
  the cursor, exactly as `files` does.

```
UsbMsg::List                          → Devices { Vec<UsbDevice> }
UsbMsg::Claim  { device, reason }     → Claimed { device, endpoint }
                                      | Refused { device, UsbError }
UsbMsg::Release{ device }             → Released { device }
UsbMsg::Revoked{ device, UsbError }   (host → holder, unsolicited)
UsbMsg::Attached { .. }               (holder → mesh, state replication)
UsbMsg::Data   { stream, bytes }      (Bulk; the USB/IP byte stream)
```

`UsbDevice` carries what a user needs to recognise the thing — vendor and
product ids, the strings when the device offers them, the class, the bus id on
its host, and a `UsbDeviceId` stable for as long as it stays plugged in. Not the
raw busid alone: `3-2` means nothing on another machine and changes when the
device is replugged into another port.

Errors are values, never silence — the `files` rule, for the same reason. A
claim that goes unanswered hangs the user's `adb` with no way to tell whether
the device is busy, refused, or gone. `UsbError` names it: `InUse(PeerId)`,
`Gone`, `Denied`, `DriverMissing`, `NeedsPrivilege`.

State replication follows rule 5: the host broadcasts the **whole** device list
on change, so a reconnecting peer resyncs by receiving the current one rather
than replaying a history of arrivals and departures.

## Not every device survives the trip

Worth stating in the design because it sets expectations, and because the code
must report it rather than let the user discover it as flakiness.

USB/IP carries transfers over TCP, and the transfer types differ sharply in how
they tolerate that:

- **Control and bulk** — phones and `adb`, mass storage, programmers and
  debuggers, printers, serial adapters, security keys. These retry and have no
  hard deadline. They work well over a LAN.
- **Interrupt** — keyboards, mice, HID. Work, with latency added. Cursio already
  shares input properly, so this is rarely the interesting path.
- **Isochronous** — webcams, USB audio interfaces. These have a per-frame
  deadline and no retransmission; the LAN misses it. These work poorly over
  USB/IP as a matter of the transfer model, not of implementation quality.

The module therefore reports an isochronous-heavy device as
`UsbTransport::Degraded` in its listing rather than pretending. The user may
still attach it; they are told what to expect first.

`adb` — the case that motivated this — is bulk, and lands in the good category.

## Platforms

### Linux

Both halves are in mainline and need no install: `usbip-host` to export,
`vhci-hcd` to import, driven by `usbip`/`usbipd` from `linux-tools`. Present but
unloaded on the development machine (kernel 6.17: the four `usbip` modules ship
with the kernel package, and `/usr/bin/usbip` comes from `linux-tools-common`).

Two facts shape the backend:

- The modules are **not loaded by default** (`modprobe usbip-host vhci-hcd`),
  and loading them plus binding a device needs root. Cursio already asks for
  elevation on Windows for input (`elevate.rs`); on Linux this is a `polkit`
  action, requested per bind rather than by running the daemon as root.
- The kernel's own control surface is **sysfs**, not the CLI. `usbip` is a thin
  wrapper over `/sys/bus/usb/drivers/usbip-host/` and
  `/sys/devices/platform/vhci_hcd.0/`. The backend talks to sysfs directly —
  shelling out to a CLI to parse its human-readable output is the kind of thing
  that breaks on a distro's version bump.

### Windows

The importing half is `usbipd-win`, which installs a VHCI driver. It is signed
and open-source, so this is not the `cursio-vad` situation of maintaining a
driver fork — but it *is* a separate install plus a reboot before anything
works, the same shape as WinFsp for the files module.

Consequences that must be handled rather than discovered:

- Absence is a first-class state. `advert()` returns `None` when the driver is
  not installed, so the module is simply not negotiated and the peer is told why
  in the log — never a link that half-works.
- Exporting from Windows (a device plugged into the desktop, used from the
  laptop) is the weaker direction in `usbipd-win` and is gated behind its own
  capability flag rather than assumed symmetric.

### macOS

Neither half exists, and the kernel gives no supported way to add one — a
virtual host controller is a kext, and the modern system refuses that class of
extension. A Mac therefore does not advertise the module. Per the platform rule,
this is an explicit "no backend here", not a silent degradation.

## Shape of the implementation

`crates/usb` is mechanism, `crates/core/src/usb_session.rs` is policy — the
split `files` already uses:

```
crates/usb/            enumerate, bind/unbind, attach/detach, the local socket
  ├── linux.rs         sysfs + polkit
  ├── windows.rs       usbipd-win
  └── lib.rs           traits, UsbDevice, UsbError

crates/core/src/usb_session.rs
                       what may be claimed, by whom, and who holds it now
```

The CLI's only job stays constructing the registry: `--usb` decides
participation, `cursio usb` lists what the mesh can see and who holds it. No
branch in `main` names a device.

`PROTOCOL_VERSION` does not move for this. `ModuleId(7)` is a new id in an open
namespace and the module's own vocabulary is versioned by its `ModuleAdvert` —
which is the whole point of the envelope work in `platform-design.md` step 2.

## Milestones

1. **Vocabulary and policy.** `protocol/src/usb.rs`, `usb_session.rs`, ownership
   and the claim/release/revoke state machine, tested against `MemTransport`
   with a fake backend. No kernel involvement, so this is where the ownership
   races get settled.
2. **Linux export + import.** sysfs backend, `modprobe`, polkit. End-to-end
   Ubuntu↔Ubuntu with a phone and `adb`.
3. **Windows import.** `usbipd-win` detection, the missing-driver path, attach
   from the desktop to a device on the laptop. This is the milestone that
   answers the original question.
4. **Windows export**, if `usbipd-win`'s export direction proves solid enough to
   depend on.
5. **Degradation reporting** — isochronous detection in the listing, and the
   revoke path when a device is physically unplugged while a peer holds it.

## Invariants

1. **Cursio never speaks URB.** If a change requires understanding a USB
   transfer's contents, it is in the wrong layer.
2. **One holder, and only the holder releases.** A claim from a non-holder is
   refused, not honoured.
3. **The host may always revoke.** Physical unplug beats any protocol state.
4. **No listening port.** The USB/IP daemon binds loopback only; the mesh
   channel is the only way in.
5. **Absence is advertised, not simulated.** No driver, no advert. Never a
   device that appears attachable and silently does nothing.
