# Media: what plays everywhere, driven from anywhere

The `media` module (`ModuleId` 13) replaces KDE Connect's media control: each
machine publishes the players it has, and each machine shows its peers'
players where it already shows media — GNOME's media widget on a desktop, the
media controls and HyperOS's island on a phone — with buttons that act on the
machine that plays.

Like notifications, this is the *state and events of a device*, not a stream
between machines. It is still a module, because unlike `NodeStatus` it is
optional: a machine with nothing to read players from does not advertise it.

## Vocabulary (`protocol/src/media.rs`)

| Message | Direction | What |
|---|---|---|
| `Players(Vec<Player>)` | either | every player this side has, replacing the last list |
| `Command { player, command }` | either | press play/pause/next/previous, seek, volume |
| `ArtRequest { player, art }` | either | ask for the cover a list named by hash |
| `Art { art, mime, bytes }` | either | that cover, ≤ 256 KiB, or empty if gone |

**Sent when it changes, never streamed.** A list goes out when something a
remote control shows changes: a track, a state, a capability, a seek. The
position travels with it as "this far, when sent", and the receiver advances it
while playing (`RemotePlayer::position_ms`). A playing track is therefore
silent on the wire between tracks — which matters on a Bluetooth link.

**Covers are asked for.** A cover is the only large thing; most are never
looked at closely and the same one lasts a whole album. A list names it by
hash, the receiver asks once (`media_hub` remembers what it asked for 30 s, so
a resent list does not ask again), and the sender scales it down first.

Priority is `Video`: a button press is felt, so it rides above bulk, and
nothing here may wait in front of the cursor.

## Where the halves live

- `core/src/media_hub.rs` — one per process. Holds this machine's backend
  (`MediaBackend`), where peers' players are shown (`MediaDisplay`), every
  peer's last list and fetched covers, and "Disconnect".
- `core/src/media_session.rs` — the module on one link: sends this machine's
  list when the hub's generation moves, carries button presses and covers.
  Backend calls run on `spawn_blocking`: an MPRIS call or a MediaController is
  another process's answer.
- `core/src/media_mpris.rs` (Linux) — reads players with a 1 s `GetAll` poll
  (cheaper than one `PropertiesChanged` subscription per player plus one for
  players appearing, and the position is compared only for jumps), and offers
  each peer player as `org.mpris.MediaPlayer2.cursio_<peer>_<n>` on a
  connection of its own — MPRIS fixes the object path, so one connection owns
  one player. Our own names are skipped when reading, or a peer's player would
  bounce back. All of it is off the Tokio runtime (see `session_bus.rs`).
- `crates/android/src/media.rs` + `MediaBridge.kt` — the phone's players come
  from `MediaSessionManager.getActiveSessions`, which Android answers only to
  the holder of notification access, named by `CursioNotificationListener`.
  Buttons go to `MediaController.TransportControls`.
- `MediaNotifier.kt` — a computer's player becomes a `MediaSession` of the
  app's own plus a `Notification.MediaStyle` notification tied to its token.
  That pairing, not a custom layout, is what Android's media controls and
  HyperOS's island pick up — how KDE Connect's players appear there.
- `media_smtc.rs` — **Windows** reads its players from
  `GlobalSystemMediaTransportControlsSessionManager`, the sessions behind the
  system's own media flyout: track, state, timeline, buttons and cover (the
  session's thumbnail stream). It advertises the module as a `Source` only:
  Windows lets no app publish a session for a player that is not itself, so
  peers' players are not shown there.
- A player BlueZ's `mpris-proxy` stands in for — a phone or a computer
  playing through this one over Bluetooth — is skipped on Linux. It is not
  this machine's: a peer offers its own players, and forwarding the proxy
  sent them to phones a second time, under the wrong machine and without a
  cover. A cover named by a web address (Spotify's) is fetched here through
  GIO and sent like a local file, so the phone needs no network of its own.

## Who shows whom

A desktop shows every peer's players. A phone shows only computers'
(`MediaFactory::shows_only_computers`): another phone's player could only be
driven from a second phone in the other hand.

## Disconnect

A phone's "Disconnect" on a computer's player hides that player rather than
stopping anything — a video left paused in a browser tab otherwise sits in
the island all day. It comes back when its title changes: the next thing that
plays there is worth showing again.

A player paused for more than five minutes leaves the phone's shade on its
own (`MediaNotifier.STALE_MS`) and returns when it plays: a browser keeps the
player of any tab that once played a video, for as long as the tab is open.

## Several players at once

Sound is never the question: every application is a stream of its own, all of
them play at once, mixed by the system's mixer with a volume each. What needs
an answer is which player one button means.

On a phone, the main player is the one that last started playing on any
computer (`MediaNotifier.present`): only its `MediaSession` is active and its
notification held, and it is posted last, so the system's media controls and
HyperOS's island follow it instead of whichever player last changed a title.
The others stay in the shade with their own buttons. Pausing does not hand
the place on — a second press would start the player beside it. A desktop
needs nothing of ours: a peer's players are MPRIS players there, and GNOME's
media keys already go to the one that last started.

## Pausing for a call

What KDE Connect does: while any phone reports a call (`calls_hub`), the
machine the user is at (`presence::user_here`) pauses what plays there, and
resumes exactly those players once the call ends (`media_hub::CallPause`,
ticked by the MPRIS or SMTC poll). A machine the user is not at is left alone.

## Not yet

- Peers' players shown on Windows (above: no app may publish a session).
- Volume from the phone's notification: the session callback carries no
  volume; the computer's own volume is the audio module's job.
