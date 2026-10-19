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
  voice — CVSD (8 kHz linear PCM) or mSBC (16 kHz, below) — through an
  ordinary Cursio playback node and
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

### Wideband (mSBC) when this machine can carry it

CVSD is 8 kHz; mSBC is 16 kHz and is what makes a call sound like a call on
a modern phone. It is offered only where `libsbc.so.1` can set up mSBC
(`sbc_init_msbc`), checked once per process; elsewhere the unit stays
narrowband and says so in the log.

- **Negotiation** (`hands_free.rs`). A wideband unit adds codec negotiation
  (bit 7) to `AT+BRSF` and wideband speech (bit 5) to its SDP record. Against
  a phone that negotiates codecs (`+BRSF` bit 9) it lists `AT+BAC=1,2` before
  the indicators. The phone picks the codec of each audio link with `+BCS`,
  answered with `AT+BCS`; a choice this side cannot carry is answered by
  listing again.
- **The link** (`linux/sco.rs`). mSBC needs transparent air mode, so the
  controller passes the frames through. Incoming, the listener defers
  accepting (`BT_DEFER_SETUP`): the socket arrives before the link is set up,
  takes the air mode of the codec the phone just chose (`BT_VOICE`), and the
  first read accepts it. Bringing the voice back here asks the phone with
  `AT+BCC` rather than connecting, so the link carries the codec both sides
  agreed.
- **Framing** (`msbc.rs`). Each 57-byte frame (120 samples, 7.5 ms) travels
  as a 60-byte H2 packet: `0x01`, a sequence byte (`08 38 C8 F8`), the frame,
  one byte of padding. What the socket delivers is cut at the controller's
  packet size, not at frames, and a lost or corrupted packet is not marked, so
  the reader scans for an H2 header followed by mSBC's sync word `0xAD`, as
  PipeWire and oFono do. A frame that does not decode plays as silence of its
  length, so the call's timing survives a lost packet.
- **Pacing.** Each decoded frame is answered with one encoded frame of
  microphone, written in pieces the size of what the phone sends, so both
  directions stay on the phone's clock as with CVSD.
- **Falling back.** An adapter that cannot do transparent SCO accepts the
  socket option and then delivers noise. A wideband link that yields no
  decodable frame within two seconds is closed, the voice returns to the
  phone, and the unit lists CVSD alone (`AT+BAC=1`) for the rest of that
  link; the next call is narrowband.

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
buttons of its own, and two would ring twice. Only the dialer's: the phone
tells them apart by package (`TelecomManager`'s default and system dialer, and
the in-call screens that post under their own name). Holding back every call
notification held back WhatsApp's and Telegram's too, which the calls module
cannot see, and those reached no computer at all.

### A messenger's call

A WhatsApp or Telegram call is no telephony call: the call monitor does not
see it and `+CLCC` does not list it. It reaches a computer as a notification
— recognised as a call by its category, or, as Telegram posts it, by
`Notification.CallStyle` or a full-screen intent on a channel named for calls
— and the phone adds `NotifyMsg::CallButtons`: which of its buttons answer,
decline and end it. Android names those buttons' intents in `CallStyle`
(Android 12+), which is the only way to tell them apart in any language. The
computer shows it among the phone's calls, in the same window, as "who · app";
answering it presses that button on the phone.

Its voice, with a hands-free link up, comes as Android's **virtual call**: the
phone raises the `call` indicator so the headset opens audio, but `+CLCC`
lists nothing. An empty listing while the indicator says a call is up is
therefore taken as a call without a number, and the voice is steered like any
other — here when it was answered here, on the phone otherwise. That call is
shown once, under the messenger's name.

Only a phone is taken as a hands-free gateway: a Windows laptop plays that role
too (for its own Phone Link) and was shown among the calls. A device whose
Bluetooth class is not Phone is refused; one that gives no class is let
through.

## Where the voice plays: the machine the user is at

The phone pairs with one computer, which carries the call; the user may sit
at another. With `[calls] voice = "follow"` (the default) the computer
carrying the call plays the voice where the user is and records that
machine's microphone (`core/src/call_audio.rs`); `"here"` keeps it on this
computer.

- **Where the user is** (`presence::whereabouts`): the peer this machine's
  mouse drives, else here with recent input, else "away" — and away moves
  nothing, since a machine left alone cannot say where the user went. It is
  looked at twice a second, and a new answer must hold for two seconds
  before the sound moves, so a cursor crossing to a peer and straight back
  does not tear the call's audio down twice.
- **Getting there** (`core/src/audio_relay.rs`). The voice leaves as one of
  this machine's application streams on the audio module — advertised with
  no device, Opus at the module's low-latency settings, in the `Audio`
  priority class — so the peer plays it on its default output like any other
  application of ours, and nothing new crosses the wire: any build that runs
  the audio module plays it. Its label, `Call · <phone>`, is how the peer
  knows it carries a call. The microphone comes back the same way: the
  module tees the decoded PCM of the first microphone the peer offered to
  the call, and keeps that stream demanded while the call hears it.
- **Shown there.** The machine playing a relayed voice shows the phone's
  call with the voice on it (`Status.calls[].audio_here`); the machine
  carrying it shows where it went (`voice_on`). The call window says "voice
  on <machine>".
- **The phone's music** over A2DP follows the same way, under the phone's
  name.
- **Taken where?** A call answered in the window on the screen this
  computer's mouse drives reaches the phone over the mesh, and the computer
  carrying the voice only sees it go up. So a call going up while the user's
  hand was on this desk in the last fifteen seconds (`presence::at_desk`) counts
  as taken here; one picked up with the desk untouched was taken on the
  phone, and its voice stays there.

## Settings: `[calls]`

| Field | Default | |
| --- | --- | --- |
| `answer_moves_voice` | `true` | Answering or dialling from a computer brings the voice to it. Off, nothing steers the voice. |
| `voice` | `"follow"` | `"here"` or `"follow"` (the machine the user is at). |
| `output` | unset | The output the voice plays on here, by device id; the default output when unset. |
| `input` | unset | The microphone the call hears here. |

All four apply at the next call without a restart. The app's settings page
shows them beside what this computer can do (`Status.call_setup`): whether
`cursio bluetooth install` handed the roles over, whether the hands-free
unit and the speaker registered (with BlueZ's reason when not), the phones
paired with it, and the outputs and microphones to pick from — read every
twenty seconds on a thread of their own, since BlueZ's and PipeWire's
blocking calls cannot run on the runtime. `headset` says whether this
computer can be a headset at all (Linux only, so far): where it cannot, the
page says so in one line instead of offering a Linux command and three rows
that can never turn green, and keeps the four settings above, since a call
another computer carries can still send its voice here.

## Where it shows

- **Notifications** (`calls_display.rs`), through the same notification
  server as the phone's notifications: urgent while ringing, replaced in place
  as the call moves on, closed when it ends. Closing a call's notification is
  not declining it.
- **The control socket**: `Status.calls`, and `Command::Call { phone, action }`
  for the app and the GNOME menu.
- **The CLI**: `cursio call list | answer | decline | hangup | dial | mute |
  audio`.
- **The app's Messages page** dials: a call button on a conversation with one
  number, and on a new message once one number is typed — the phone places
  the call, and the voice comes here as for any call dialled from a computer.

## Not here yet

- Bluetooth on Windows and macOS: their stacks own the HF and A2DP sink roles
  and offer no user-space way to take them. Calls there work over the mesh,
  voice on the phone.
- Following a user to a machine this one does not drive: a laptop worked on
  with its own keyboard is invisible from here, because desktops tell each
  other nothing about their user. The voice then stays where it was.
- AAC, as above.
