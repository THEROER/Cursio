# Calls and the phone's sound over Bluetooth

A phone's calls appear on the computer the user is at: who is calling, with
Answer and Decline; while talking, Hang up, Mute and where the voice is. A
call can be placed from the computer. And when the computer is paired with
the phone over Bluetooth, the voice — and, if the user picks the computer as
the phone's speaker, all of the phone's sound — plays on the computer's own
output and microphone, through Cursio.

## Why the voice needs Bluetooth

Android gives an ordinary app no way to record or inject a call's audio.
`AudioSource.VOICE_CALL` and the `CAPTURE_AUDIO_OUTPUT` permission are
reserved to system apps, and playback capture (`MediaProjection`) excludes
call audio by design. So over the mesh — Wi-Fi — the voice cannot leave the
phone, however the rest of the phone's sound does.

The one route the phone itself offers is the one every car kit and headset
uses: the Hands-Free Profile. A paired hands-free unit gets the call's audio
over SCO and controls the call with AT commands. That is the "normal protocol
for talking" — and no permission on the phone is involved.

## Cursio is the headset, not the desktop

Phone Link and GSConnect let the operating system's Bluetooth stack be the
headset. On Linux that is PipeWire's `hfp_hf` role, which makes the phone a
*Bluetooth headset device* on the desktop for the length of the call: it
appears, becomes the default output and microphone, and plays the call
wherever the desktop's policy puts a new device.

Cursio takes the role itself instead (`crates/bluetooth`):

- It registers the HF profile with BlueZ (`ProfileManager1.RegisterProfile`,
  UUID `0x111E`, HFP 1.7) and receives each connecting phone's RFCOMM socket
  in `Profile1.NewConnection`.
- It runs the service level connection over that socket (`hands_free.rs`):
  `AT+BRSF`, `AT+CIND=?`, `AT+CIND?`, `AT+CMER`, then `+CLIP` caller id and
  `AT+CLCC` call listings, one command at a time. Answer, hang up and dial are
  `ATA`, `AT+CHUP`, `ATD<number>;`.
- While a call is up it listens for the phone's SCO connection and carries the
  voice: CVSD, 8 kHz linear PCM, through an ordinary Cursio playback node and
  the same microphone tap that shares a microphone with peers
  (`core/src/call_audio.rs`). The computer keeps its devices; nothing appears
  or disappears on the desktop.

"Use phone" closes Cursio's end of SCO, which is how any headset hands the
voice back; "Use computer" opens it again from this side.

### Taking the roles from PipeWire

BlueZ allows one owner per profile, and PipeWire registers every role by
default — its `hfp_hf` is why the first attempt answered `UUID already
registered`. `cursio bluetooth install` writes
`~/.config/wireplumber/wireplumber.conf.d/90-cursio-bluetooth.conf` setting
`bluez5.roles = [ a2dp_source bap_source hfp_ag hsp_ag ]` and restarts
WirePlumber. PipeWire keeps every role in which the computer plays *to*
headphones and speakers, so those work as before; it gives up the roles in
which the computer *is* a phone's headset or speaker. The restart drops
Bluetooth audio for a second, so it is done by that command only — a running
node never restarts WirePlumber. `cursio bluetooth uninstall` removes the file.

### SCO and PipeWire's listener

PipeWire listens for incoming SCO on the wildcard address for its gateway
role. The kernel hands an incoming SCO connection to the listener bound to
the *exact* local address before the wildcard one (checked on Nocturne:
binding the wildcard fails with `EADDRINUSE`, binding the adapter's address
succeeds). So Cursio binds the adapter's address and takes the phone's voice
without touching PipeWire. Because the more specific listener would also take
a headset's SCO, it exists only while a phone has a call, and anything else
that connects in that window is refused with a warning naming it.

### Narrowband for now

Codec negotiation is not advertised, so the phone uses CVSD (8 kHz). Wideband
(mSBC, 16 kHz) needs a transparent SCO path and an SBC codec with the H2
framing on this side; offering it before both exist would have the phone pick
a codec the audio path cannot decode.

## The phone's sound: Cursio as its Bluetooth speaker

Playback capture works over Wi-Fi but asks for screen-recording consent every
time the app process starts, and that consent dies with the process. A phone
that picks a Bluetooth speaker sends all of its sound there with no prompt at
all. So the computer is also the phone's A2DP sink — again Cursio, not
PipeWire's `a2dp_sink`, for the same reason: one owner of the phone's sound,
one route, one volume, the same way onto another machine, instead of a
PipeWire Bluetooth node the desktop routes on its own.

- `MediaSinkService` registers an SBC endpoint with `Media1.RegisterEndpoint`
  (UUID `0x110B`), picks 48 kHz joint stereo when the phone offers it, and
  acquires the transport when the phone starts playing.
- Frames are decoded by the system's `libsbc.so.1`, opened with `dlopen` when
  a stream starts: BlueZ and PipeWire already install it, no build needs its
  headers, and a machine without it loses only this, with an error naming
  `libsbc1`.
- The transport's `Volume` (AVRCP absolute volume) is the phone's own volume
  slider and is applied to the samples, so the phone's buttons keep working.
- The decoded sound plays through a Cursio playback node named
  `Cursio — <phone>`.

AAC is not offered: A2DP carries it as LATM inside RTP, which means a second
codec library; every A2DP source must offer SBC.

### Latency (estimated; not yet measured here)

A2DP buffers on the phone before it sends: Android's SBC path typically adds
150–250 ms between an app and the air, and the sink adds one packet (~20 ms)
plus the playback node's 40 ms lead. Fine for music and video (players
compensate for the reported delay), noticeable for games — where the Wi-Fi
playback capture, at around 60–80 ms end to end, stays the better path. Calls
are separate: SCO is a fixed 7.5 ms slot, so a call's voice adds only the
playback lead.

## Over the mesh: the `calls` module

Not every computer has Bluetooth, and a phone in the next room is out of its
range anyway. So the phone also reports its calls over the mesh
(`ModuleId::CALLS`, `protocol/src/calls.rs`), and a computer that is not its
hands-free unit acts on them through it — with the voice staying on the phone
(or its own headset).

- **State, not events.** `CallsMsg::Calls` carries every call the phone has
  now and is resent whole on change; a call that ended is absent. A link that
  comes up mid-call is right at once.
- **The phone side** (`CallMonitor.kt`) is not the default dialer, so it sees
  calls only through the `PHONE_STATE` broadcast: idle, ringing (with the
  number, given `READ_CALL_LOG`), off-hook. That is aggregate, so at most the
  call that is up and one waiting call are modelled — every case a person
  handles from a computer. Answer and end go through `TelecomManager`
  (`ANSWER_PHONE_CALLS`), dialing through `placeCall` (`CALL_PHONE`), mute
  through `AudioManager`. Names come from contacts (`READ_CONTACTS`).
  Becoming the default dialer would give every call by name, and would take
  the phone's own dialer away from the user; that trade is not made.
- **A refusal carries its reason** (`CallsMsg::Refused`): "allow Cursio to
  answer calls", not silence.

### One phone, two routes

A computer may hear a phone both ways. `core/src/calls_hub.rs` shows it once:
by its hands-free link while one is up (that is where the voice goes, and its
state is the phone's own call list), by the mesh otherwise. They are matched
by name — the phone reports its device name, which is what it is called over
Bluetooth, because Android hides its own Bluetooth address from apps. A
request goes by whichever route reaches the phone.

While the phone reports calls itself, the dialer's own call notification no
longer crosses through the notify module: the computer shows the call with
buttons of its own, and two would ring twice.

## Where it shows

- **Notifications** (`calls_display.rs`), through the same notification
  server as the phone's notifications: urgent while ringing, replaced in place
  as the call moves on, closed when it ends. Closing a call's notification is
  not declining it.
- **The control socket**: `Status.calls`, and `Command::Call { phone, action }`
  for the app and the GNOME menu.
- **The CLI**: `cursio call list | answer | decline | hangup | dial | mute |
  audio`.

## Not here yet

- Bluetooth on Windows and macOS: their stacks own the HF and A2DP sink roles
  and offer no user-space way to take them. Calls there work over the mesh,
  voice on the phone.
- Carrying a call's voice or the phone's Bluetooth sound on to another
  machine. Both enter the audio path as ordinary playback nodes, so the next
  step is offering them to peers like any local stream.
- mSBC wideband and AAC, as above.
