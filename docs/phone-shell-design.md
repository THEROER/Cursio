# A phone's own shell

An Android app is refused three things Cursio needs from a phone it is already
trusted with:

- **another app's sound** without the screen-recording consent: playback
  capture is tied to a media projection, which asks on every start;
- **the screen** without asking every time, for the same reason;
- **an application's update** without its person tapping Install.

The `shell` user — what `adb shell` runs as — has all three. scrcpy records
sound through `REMOTE_SUBMIX` (or playback capture) and the screen through a
display of its own, and `pm install` asks nobody. Wireless debugging
(Android 11+) gives that user to any program that paired with the phone once.
The program can be on the phone itself, as LADB and Shizuku are; so the
phone's node reaches its own `shell`, with no computer and no cable.

## How it fits

1. The person turns on **Wireless debugging** once and types, in Cursio, the
   six digits its "Pair device with pairing code" screen shows. Cursio's key
   (RSA, generated on the phone, kept in its private files) is then on the
   phone's **Paired devices** list, where the person can remove it.
2. The node connects to the phone's own debugging port and gets `shell`.
   The port is found by trying the ports wireless debugging picks from
   (30000–49999) on the phone's Wi-Fi address and asking each open one to
   run `true` with Cursio's key — about a second — and kept until it stops
   answering, since it changes only when debugging is turned on again.
   Android's mDNS was the plan and is still listened to, but on HyperOS 3 /
   Android 16 it does not show an app the services its own phone
   advertises: discovery ran and found neither. For the same reason the
   pairing notification asks for the port beside the code (both are on the
   pairing dialog), and takes the code alone only when mDNS did show it.
3. Through that connection it starts a helper from its own APK as `shell`
   (`app_process`, the way scrcpy starts its server). The helper does what
   only `shell` may: sound, screen, `pm install`. It speaks to the node over
   the same ADB stream it was started on — no socket of its own, since an
   app may not connect to `shell`'s abstract sockets and should not leave a
   TCP port open.
4. When debugging is off (after a reboot, a change of network), the node says
   so and falls back to what an app may do: projection with its consent, the
   installer with its tap. Turning debugging back on is the person's one tap;
   the key stays trusted.

The protocol is `libadb`'s (`crates/phone-shell`): TLS 1.3, SPAKE2 bound to
the TLS exporter, the encrypted peer-info exchange, then ADB over TLS.
`cargo run -p cursio-phone-shell --example shell_probe` pairs and runs one
command from a computer — whether a phone of a given make accepts it.

## Security

- **The debugging port**, while on, is open to the Wi-Fi network but admits
  only paired keys over TLS; pairing a new key needs the code on the phone's
  own screen. That is Android's own model of wireless debugging, and Android
  shows a persistent notification while anything is connected.
- **Cursio's key is the phone's shell**: installing applications, granting
  permissions, capturing the screen, injecting input, reading every app's
  logcat. It does not read other apps' private data. It lives in Cursio's
  private files, which neither backup nor device-to-device transfer copies
  (`cursio_no_extraction.xml`).
- **Machines of the mesh get no shell.** The node runs named things through
  it — install a build from a builder this phone trusts, capture sound or the
  screen for a viewer already allowed — never a command a peer wrote. A raw
  command for development, if ever, would take its own time-limited consent,
  as DevTools do.

## What it costs, and where it does not work

- Some banking apps refuse to run while debugging is on, and sound through
  `shell` needs it on as long as sound is shared.
- HyperOS lets ADB install only with **Install via USB**, and inject input
  only with **USB debugging (Security settings)**; both need a Mi account.
- Wireless debugging needs a Wi-Fi network: not on mobile data, and not on a
  phone that is itself the hotspot.
- HyperOS turns wireless debugging off after a reboot and a change of
  network.

## Order of work

1. `crates/phone-shell`: pairing and a command over TLS, and the probe that
   tries them against a real phone (**built**). Run against a POCO X7 Pro on
   HyperOS 3 / Android 16 from a computer: a fresh key paired by its code,
   and a command over TLS on the debugging port ran as `uid=2000(shell)`.
   Commands go through ADB's `exec:` service — `sh -c`, no terminal, output
   byte for byte — not `cmd`, which names a binder service.
2. Pairing from the phone's own window, the key in its files, finding the
   port by mDNS, and the node's connection with its fallback when debugging
   is off. **Built, and run on a POCO X7 Pro (HyperOS 3)**: paired from the
   phone's own window, the node ran `id -un` through its own debugging port
   and got `shell`. The key could not be kept by `libadb`'s store at first:
   it publishes with a hard link, which Android's SELinux refuses an app.
   Settings → "This phone's
   shell" → Pair opens the developer settings at wireless debugging and
   starts looking for the pairing service (`ShellAccess.kt`, NsdManager,
   only addresses of this phone); when it appears a notification asks for
   the code with an inline field (`ShellPairReceiver`), and
   `nativeShellPair` pairs from Rust (`crates/android/src/shell.rs`, the key
   under the node's data directory, `shell/`). The debugging port is
   watched for as long as the node runs; once paired the node runs `id -un`
   through it and says the answer in its log, readable from a computer.
3. `pm install` for builds from a trusted builder, with no tap. **Built for
   applications' builds**: `offer_app_to_installer` installs through the
   shell when it is paired (`cursio_phone_shell::install`, the APK streamed
   down the command's stdin as `adb install` does — `cmd package install -r
   -S <size>` — since `shell` cannot read Cursio's files), and falls back to
   the installer's notification when debugging is off or the package
   manager refuses (a different signing key, which the installer's check
   then names). The phone notices the install within 3 s, so `apps ship`
   returns or opens the application on its own. Streaming measured from a
   computer: Cursio's own APK replaced in 5 s with nothing asked. Cursio's
   own builds still go through the notification: installing them replaces
   the running node, which should wait for a quiet moment, as a computer's
   switch does.
4. Screenshots by `screencap -p` (**built**): a computer asks
   `DiagRequest::PhoneShot` and the phone answers the PNG its shell took
   (`phone_hands::screencap`), so `cursio phone … screenshot` and MCP
   `phone_screenshot` need no consent, open no window on the computer and
   show the screen as it is, not as a stream seconds behind. A phone that
   cannot says why; an older one does not answer, and the computer takes the
   picture from the stream as before. 1220×2712 in 0.7 s, measured.
5. Touch through the shell's `input` (**built**, `ShellStrokes` in
   `window-session/src/phone_gesture.rs`, carried out by `android/src/screen.rs`). An
   app may close its screens to accessibility services, which Android 14+
   honours by dropping their injected touch — a bank's documents opened to
   `input tap` and not to the same tap from `ScreenTouch`. Each `input` is a
   process of its own, a few hundred milliseconds, so a stroke is held and
   played whole when the finger lifts: a tap, a long press, or a swipe as
   long as the drag was. Pinching and text stay with the accessibility
   service. `input` prints nothing when it injected; anything it prints is a
   refusal (debugging off, or HyperOS' "USB debugging (Security settings)"
   off), after which the service carries touch for 30 s before the shell is
   tried again.
6. The `shell` helper: sound by `REMOTE_SUBMIX`/playback capture, then the
   screen as a stream, both without the projection's consent.
