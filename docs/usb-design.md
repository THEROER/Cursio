# USB Design (one device, every machine in the mesh)

Status: **designed; vocabulary and ownership built, arbitration not.** Companion
to `files-design.md`, whose FUSE layer is the model this follows, and
`platform-design.md`'s rules for new modules.

## What this is for

A phone is plugged into the laptop; `adb devices` on the desktop lists it, and
so does `adb devices` on every other machine. A programmer, a security key, a
printer — whichever machine has the port, every machine has the device. The USB
port stops being a property of one box and becomes a property of the mesh.

Deliberately not a per-device feature. There is no adb module and no printer
module: the shared thing is the bus, and everything speaking USB rides it.

## The driver is an adapter, not a tunnel

This is the decision everything else follows from, and the first draft of this
document got it wrong.

The obvious way to carry USB is to take USB/IP whole — both its transport *and*
its semantics — and move the bytes:

```
vhci-hcd ──[opaque bytes]── Cursio ──[opaque bytes]── usbip-host
```

Cursio is a pipe. Nothing can be answered, merged or arbitrated, because
nothing is understood. From inside that shape "one holder at a time" looks like
a law of USB. It is not: it is a consequence of choosing to be a pipe.

`crates/files` already rejects that shape. FUSE is not a tunnel to a remote
filesystem; it is an **adapter** that turns kernel intent into requests Cursio
understands:

```rust
fn read(&self, ino, fh, offset, size, reply) {
    match self.peer.read(remote, offset, size) { ... }
}
```

`read(handle, offset, len)` is an *operation*, not a byte stream, which is
precisely why any number of programs read a mounted volume at once. The server
decides how to satisfy each one.

USB/IP affords the same treatment, because a URB is also an operation rather
than raw material. The kernel's own protocol
([USB/IP protocol](https://docs.kernel.org/usb/usbip_protocol.html)) puts the
meaning in a fixed 48-byte header:

| Offset | Field | What it says |
|---|---|---|
| 0x00 | `seqnum` | which request a reply belongs to |
| 0x00 | `direction`, `ep` | in or out, and which endpoint |
| 0x14 | `transfer_flags` | |
| 0x18 | `transfer_buffer_length` | |
| 0x28 | `setup` (8 bytes) | the control request itself, when `ep` is 0 |

The constants needed to read those fields are uapi, not folklore:
`/usr/include/linux/usbip.h` defines the direction bit (`USBIP_URB_DIR_IN`
`0x0200`, `USBIP_URB_DIR_OUT` `0`, `USBIP_URB_DIR_MASK`) inside
`transfer_flags`, and `/usr/include/linux/usb/ch9.h` names the standard
requests the arbiter has to tell apart — `USB_REQ_GET_DESCRIPTOR` `0x06`,
`USB_REQ_SET_CONFIGURATION` `0x09`, `USB_REQ_SET_INTERFACE` `0x0B`. Both ship
with the kernel headers, so nothing here is reconstructed from observation.

So the shape this design takes is the third one — neither a pipe nor a
service-specific proxy:

```
desktop  vhci-hcd ─┐
tablet   vhci-hcd ─┼→ Cursio (arbiter) → usbip-host → device
laptop   vhci-hcd ─┘
```

Three virtual host controllers, three kernels, each holding what it believes is
its own device. Cursio reads the header of every URB and decides what happens to
it. **Every machine in the mesh sees the device and can use it**; what varies is
how a given transfer is satisfied.

## What arbitration actually does

Three classes of traffic, told apart by the header alone. No USB stack, no
descriptor parsing beyond the eight setup bytes.

### Descriptors: answered from cache, to everyone

Enumeration is control transfers on endpoint 0 — `GET_DESCRIPTOR` and friends —
and every host asks the same questions and must receive the same answers. They
are pure reads of data that does not change while the device is plugged in.

Cursio answers them itself, from a cache filled on first enumeration. The device
is not involved at all, which means a machine attaching for the second, third
and fourth time costs the device nothing and cannot disturb whoever is already
using it. This is what makes "every machine sees it" cheap rather than a
thundering herd.

### Data transfers: serialised, and the reply goes to whoever asked

Bulk and interrupt transfers are ordinary work. They queue, the device performs
them one at a time as it always has, and `seqnum` routes each reply back to the
host that submitted it.

Nothing is lost by serialising: a USB device executes one transfer at a time in
any case, and the queue Cursio keeps is the same queue the host controller
would. Two machines reading a device see the latency of sharing, not a failure.

### State-changing requests: intercepted

`SET_CONFIGURATION`, `SET_INTERFACE`, `CLEAR_FEATURE` and device resets change
what the device *is*, not what it is doing. Left alone, a second host's
`SET_CONFIGURATION` would reset the device out from under the first mid-transfer
— the real hazard behind USB's one-host rule.

These are recognised in the setup bytes and answered by Cursio: success when the
device is already in the requested state, refusal when honouring it would
disturb another machine. Only when a request would genuinely change the device
does exclusivity come into play — and that is a property of *those requests*,
not of the device.

The exclusivity USB really imposes therefore shrinks from "the whole device" to
"a handful of control requests" — the same narrowing FUSE achieves, where
readers are unlimited and only writes need ordering.

## Storage takes the filesystem path, and still looks like a disk

A removable disk is the one device class where carrying the bus is both
unnecessary and unsafe, and where the obvious alternative appears to cost the
user something. It does not have to.

### Why the bus is the wrong layer for it

A filesystem's structure lives on the medium, and each host that mounts it
caches that structure in its own kernel. Two mounts of one medium means two
caches with no way to learn of each other's writes — the condition storage
people call *concurrent write sharing*, and the reason a single iSCSI target
mounted by several initiators corrupts unless a cluster filesystem coordinates
them. ext4, FAT and NTFS assume sole ownership; they have no such coordination
and cannot acquire it.

Arbitrating URBs does not help. Cursio can serialise two `WRITE(10)` commands
perfectly and the medium still ends up inconsistent, because the stale data is
in a *kernel*, not in the transfers. This is the one place where the adapter
model reaches its limit — not for lack of understanding the traffic, but
because the shared state is somewhere the traffic never goes.

### The medium is mounted once, by the machine holding it

So a removable disk is not exported as a bus device. Its host mounts it exactly
as it always would — udisks does this automatically, before any peer is even
aware of it — and offers the *filesystem* to the mesh:

```
laptop:   /dev/sdb → one filesystem → /media/user/MY_USB
                                          ↓ files module
desktop:  ~/Cursio/MY_USB     tablet:  ~/Cursio/MY_USB
```

One filesystem, one cache, one authority. Every machine reads and writes at
once, and nothing can diverge because there is nothing to diverge from. The
concurrency the bus could never give is free here.

It also survives the host losing interest: unmounting in a file manager, or
closing it, does not withdraw the volume. The host is *serving* the medium, not
holding it, so a peer keeps working until the stick is physically pulled.

### It must still look like a disk

The reason this is not a compromise is that the mount is presented as what it
is. A volume arriving over the link is a removable disk on the machine that
receives it — sidebar entry, label, capacity, eject button — and applications
open it by an ordinary path with no idea a network is involved. That is the same
claim the window module makes about a peer's window, and the file module already
makes about a peer's disks; storage simply has to carry the volume's identity
across so the receiving side can dress it correctly.

The wire type already carries what that needs: [`Volume`](../crates/protocol/src/files.rs)
has `label`, `writable`, `total_bytes` and `free_bytes`, and `FilesMsg::Volumes`
is documented as being re-sent "if the set changes (a USB disk appearing)".
What is missing is the announcement when it does, a mount point of the volume's
own rather than a folder deep inside the peer's root, and the mount options that
make a file manager treat it as a drive — `x-gvfs-show` on Linux, a drive letter
on Windows, which WinFsp gives for free.

So the user's mental model — "I plugged a stick into the laptop and it appeared
on the desktop" — is exactly what happens. The transport underneath is this
project's own protocol rather than a bus, and that is invisible where it should
be.

### What the bus is still for

Raw export remains for the cases where there is no filesystem to protect:
imaging with `dd`, partitioning, flashing, a filesystem the host cannot mount,
or a medium that must be booted from. Those need the device, not the files —
and having no mounted filesystem, they have nothing to corrupt.

The rule is therefore mechanical rather than a matter of judgement: **a mass
storage device with a mounted filesystem is offered through `files`; one without
is offered through the bus.** No heuristic about what the user meant, and no
window in which both paths are live.

## What still cannot be shared

Two more limits, stated plainly because arbitration is not magic.

**Protocols that assume a single client.** `adbd` keeps one session over its
bulk pair; two hosts interleaving on it get a desynchronised stream. The right
layer there is the adb server, which
[multiplexes clients by design](https://developer.android.com/tools/adb) — so
the mesh reaches a phone by forwarding that, not by splitting its endpoints.

**Isochronous endpoints.** Cameras and audio interfaces have a per-frame
deadline and no retransmission; TCP guarantees delivery but not time. This is a
property of the transfer type, not of the implementation, and is reported as
`UsbTransport::Degraded` rather than discovered as stutter.

So the mesh offers three levels, and a device uses the highest one that fits:

| Level | Concurrency | For |
|---|---|---|
| Filesystem (`files`) | full | storage |
| Service (adb, IPP) | full | phones, printers |
| **Bus (this module)** | **full for reads, arbitrated for state** | everything else |

## Ownership, restated

The first draft made ownership the centre of the design: one holder, everyone
else refused. Under arbitration that is far too strong — it refuses machines
that only ever wanted to read a descriptor.

What is owned is not the device but its **configuration**. So:

- Any number of machines **attach**. Attaching is not a claim; it enumerates
  from cache and costs the device nothing.
- One machine at a time holds the **configuration lease**, and only while it
  needs one. A lease is taken by issuing a state-changing request, not by
  asking in advance, and is released when its transfers finish.
- The **host** — the machine with the port — arbitrates, is the single writer
  for lease state, and may always revoke. An unplug beats any protocol state.

Rule 4 in `platform-design.md` ("one owner per piece of state") still holds; the
piece of state is now the device's configuration rather than the device.

`UsbMsg::Claim`/`Release` survive with this narrower meaning, which is why the
vocabulary needed no reshaping — only `Refused` becomes rare, because the
common case no longer competes.

## Why the mesh is worth carrying it

`usbipd` listens on TCP 3240 with **no authentication whatsoever**: whoever
reaches the port takes the device. Carried inside the mesh it binds loopback
only, and every attach is authenticated as a peer that has completed SPAKE2
pairing. Discovery comes free — `usbip attach` needs a host, and the mesh
already knows every peer and survives their coming and going.

## Platforms

### Linux

Both halves are in mainline: `usbip-host` to export, `vhci-hcd` to import,
driven through sysfs. Verified on the development machine (kernel 6.17): both
modules load, `vhci-hcd` offers 16 ports, and the `status` format the port
parser reads is the one documented below.

The first field of a `status` line is the hub's speed class (`hs`), **not** the
port number — reading it as the port makes every line unparseable and so every
port busy, which surfaces as "no free port" rather than as a wrong answer.

Two facts shape the backend:

- The modules are **not loaded by default** (`modprobe usbip-host vhci-hcd`) and
  binding needs root — a polkit action per bind, not a daemon running as root.
- The kernel's control surface is **sysfs**, not the `usbip` CLI, whose output
  is formatted for people and has changed between releases. `usbip` reads sysfs
  itself; so does this.
- The kernel already tracks whether an exported device is in use and publishes
  it (`SDEV_ST_AVAILABLE`, `SDEV_ST_USED`, `SDEV_ST_ERROR` in
  `linux/usbip.h`). Lease state is read from there rather than mirrored in a
  second place that can disagree with it.

### Windows

The importing half is `usbipd-win`, a signed open-source VHCI driver — not a
fork to maintain, but an install plus a reboot, the same shape as WinFsp for
files. Verified absent on the peer; `winget` is available to install it.

Absence is a first-class state: `advert()` returns `None` when the driver is
missing, so the module is not negotiated and the log says why.

### WSL

Microsoft ships `usbip-core.ko`, `usbip-host.ko` and `vhci-hcd.ko` in the WSL2
kernel (confirmed on the peer, 6.6.87.2-microsoft-standard-WSL2) because
`usbipd attach --wsl` is the supported way to give WSL a USB device. Useful as a
test bed. It is **not** a target: a device attached into WSL is invisible to
Windows itself, which is the opposite of what this module is for.

### macOS

Neither half exists and the kernel offers no supported way to add one — a
virtual host controller is a kext. A Mac does not advertise the module.

## Shape of the implementation

```
crates/usb/                mechanism
  ├── linux.rs             sysfs enumeration            (built)
  ├── offer.rs             ports → ids peers can quote  (built)
  ├── arbiter.rs           URB header parsing, cache, leases  (built)
  ├── urb.rs               the 48-byte header           (built)
  ├── export.rs            device → usbip-host          (built)
  ├── import.rs            peer device → vhci-hcd       (built)
  ├── pipe.rs              the socketpair both kernels drive  (built)
  └── lib.rs               traits, LocalDevice

crates/core/src/usb_session.rs
                           policy: who attaches, who leases   (built)
                           LinuxUsbBackend, UsbFactory        (built)
```

The CLI flag is `--usb`, and the module is advertised only where the kernel can
actually export: `UsbFactory::advert` returns `None` with a log line naming the
`modprobe` when `usbip-host` is absent. Invariant 6, in the one place a user
meets it.

`PROTOCOL_VERSION` does not move. `ModuleId(7)` is a new id in an open
namespace, and the module's vocabulary is versioned by its `ModuleAdvert`.

## Milestones

1. ~~**Vocabulary**~~ — `protocol/src/usb.rs`, `ModuleId::USB`.
2. ~~**Ownership policy**~~ — `usb_session.rs`, tested against a fake backend.
   Written under the pipe model; the lease narrowing above is a change to what
   `Claim` means, not to its shape.
3. ~~**Linux enumeration**~~ — sysfs, verified against real hardware.
4. ~~**The arbiter**~~ — the 48-byte header, the descriptor cache, serialised
   data transfers, intercepted state changes. `urb.rs` and `arbiter.rs`.
5. ~~**Linux export**~~ — `export.rs`: unbind, `match_busid`, bind, in that
   order, and undone on failure. Privilege is reported, never escalated.
6. ~~**Attaching half**~~ — `import.rs`: a free `vhci-hcd` port and the
   socketpair whose far end the kernel drives.
7. ~~**Carrying URBs**~~ — `pipe.rs`: a socketpair whose far end goes to the
   kernel (`usbip_sockfd` when exporting, `attach` when importing) and whose
   near end is read on its own thread, since a cancelled read leaves the
   USB/IP stream misaligned. `UsbBackend` gained `collect`/`ended` so the
   session pump can send a transfer's last bytes *before* the detach that
   follows them.
8. **Windows import** — `usbipd-win` detection and the missing-driver path.
9. **Degradation reporting** — isochronous flagged, revoke on unplug.

Storage is not on this list: it belongs to `files`, which needs three additions
of its own — announce a removable volume when udisks mounts one, give it a mount
point rather than burying it in the peer's root, and mount it with the options
that make a file manager show a drive. None of them need a driver, a kernel
module or elevated privilege, which is why they land before any milestone here.

Milestone 8 is the last platform gap: a Windows peer can host devices for the
mesh today only in the sense that nothing there imports them yet. Everything
Linux-to-Linux is in place and untested against real hardware.

## Invariants

1. **Cursio reads headers, never payloads.** Enough to route and arbitrate;
   never enough to interpret what a device means by its bytes.
2. **Descriptors are answered from cache.** Enumeration must never reach the
   device once it is known, or attaching would disturb whoever is using it.
3. **One configuration lease at a time**, held only while transfers need it.
4. **The host arbitrates and may always revoke.** Physical unplug wins.
5. **No listening port.** USB/IP binds loopback; the mesh channel is the only
   way in.
6. **Absence is advertised, not simulated.** No driver, no advert — never a
   device that appears attachable and silently does nothing.
