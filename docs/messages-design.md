# Messages design

A phone's SMS conversations, read and answered from a desktop — what Phone
Link and GSConnect call messaging. Module `messages`, `ModuleId` 11.

## What crosses, and when

The phone holds the messages. Android keeps every SMS and the text of every
MMS in its telephony provider, and an app with `READ_SMS` may read all of it.
Nothing is copied to the desktop ahead of time: years of messages are far
larger than anything worth sending on the chance someone looks, and most
links never open the page.

So the exchange is request and response, like file sharing
(`protocol/src/messages.rs`):

| desktop asks | phone answers |
| --- | --- |
| `ListConversations { limit, before_ms }` | `Conversations` — newest first |
| `ListMessages { thread_id, limit, before_ms }` | `Messages` — newest first |
| `Send { addresses, body }` | `Sent` — handed to the radio |
| `MarkRead { thread_id }` | `Done`, or a refusal (below) |

The one thing the phone sends unasked is `Changed`: "something in the
provider moved, look again". A desktop showing the messages page asks again;
the status carries a `revision` per phone for that. A new SMS still appears on
the desktop at once without this module, because it is a notification, and the
notification module shows it with its reply field.

Pages run backwards in time and are cut by date (`before_ms` = the oldest
item shown), not by offset: a conversation is read from its end, and a date
boundary stays correct while new messages arrive.

Messages ride `Priority::Video`: below input and audio, which a page of history
must never stutter, above bulk, because someone is waiting on the page and a
file copy should not stand between them.

## Roles

Asymmetric. The phone advertises `Source` with its store
(`MessagesFactory { store: Some(..) }`), a desktop `Sink`
(`view: true`). Two desktops have no messages for each other, and negotiation
leaves the module off between them.

## What the phone cannot do

- **Mark read.** Since Android 4.4 only the default SMS app may write to the
  provider. `MarkRead` is attempted and refused with that reason when the
  update changes nothing; the desktop page asks quietly and ignores the
  refusal, since the phone's own app marks the conversation read when it is
  next opened there.
- **Attachments.** An MMS picture or recording is not carried. A message that
  had one says `attachment: true`, and the desktop shows that something is
  missing rather than an empty bubble.
- **Delivery reports.** A sent message is written to the provider by the
  system (a non-default app's sends are recorded for it), queued then sent or
  failed; the observer sees that change, so the desktop shows the state the
  provider holds rather than one this module invents.

## Requests are untrusted input

A peer may drive this machine's cursor; that is not leave to make the phone
text a thousand numbers. `messages_session::check` bounds every request before
the store sees it: a page is at most `MAX_PAGE` items, a body at most
`MAX_BODY_BYTES`, at most 20 recipients, and an address only phone-number
characters.

## Android

`SmsStore.kt` answers one JSON request per call (`CursioNative.onSmsRequest`,
reached from `crates/android/src/messages.rs`), because a single JNI entry
point is the one R8 must keep and `org.json` builds nested lists far more
easily than Java arrays. Changes are noticed with a `ContentObserver` on
`content://mms-sms/` rather than the `SMS_RECEIVED` broadcast: the broadcast
covers only incoming SMS, while the observer also sees a sent message turn
from queued to sent or failed, and MMS.

Permissions: `READ_SMS`, `SEND_SMS`, `READ_CONTACTS` (names in the
conversation list), asked from the phone's setup card ("Text messages on your
computer"). `android.hardware.telephony` is declared optional so a tablet
without a SIM still installs.

## Desktop

`cursio sms list|read|send` and the app's Messages page, both through the
control socket (`list_conversations`, `list_messages`, `send_sms`,
`mark_sms_read`, control v10). These wait on the phone, so the socket answers
them off the connection's loop: status pushes and quick requests behind them
are not held for the seconds a phone may take.

`cursio-app --page messages` opens the app on that page; a second launch hands
its arguments to the running app (single instance) rather than starting
another tray.
