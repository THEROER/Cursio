# cursio-channel

Several kinds of traffic over one link between two parties: the urgent kind
never waits behind the bulky kind, and the link survives its connection
dropping, moving and coming back.

You declare classes (highest priority first, each with a byte budget) and the
lanes they ride; implement `Envelope` for your message type; open each
connection with a `Trust` (`NoisePsk` ships here) over a `Carrier` (TCP and an
in-memory pair ship here); and run a `ResumableLink` over the paths. Reliable
messages are numbered, acknowledged and resent on a connection that joins the
session, exactly once; fresh ones are never replayed.

Extracted from Cursio's transport, which is its first user. The design is
argued in `docs/channel-design.md` of the Cursio repository.

Licensed under either of Apache License 2.0 or MIT, at your option.
