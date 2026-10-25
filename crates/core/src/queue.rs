//! Queue policies shared by the session and module hosts.
pub use cursio_queue::{channel, Receiver, SendError, Sender, Stats};
pub(crate) fn message_bytes(message: &cursio_protocol::Message) -> usize {
    use cursio_protocol::Message;
    let payload = match message {
        Message::Module { payload, .. } | Message::Opaque(payload) => payload.len(),
        Message::WindowFrame(frame) => frame.data.len(),
        Message::AudioChunk(chunk) => chunk.data.len(),
        _ => bincode::serialized_size(message).unwrap_or(u64::MAX) as usize,
    };
    payload.saturating_add(256)
}

pub(crate) fn merge_motion(
    previous: &mut cursio_protocol::InputEvent,
    next: &cursio_protocol::InputEvent,
) -> bool {
    use cursio_protocol::InputEvent::PointerMotion;
    if let (PointerMotion { dx, dy }, PointerMotion { dx: nx, dy: ny }) = (previous, next) {
        if let (Some(x), Some(y)) = (dx.checked_add(*nx), dy.checked_add(*ny)) {
            *dx = x;
            *dy = y;
            return true;
        }
    }
    false
}

pub(crate) fn merge_messages(
    previous: &mut cursio_protocol::Message,
    next: &cursio_protocol::Message,
) -> bool {
    use cursio_protocol::Message::Input;
    match (previous, next) {
        (Input(previous), Input(next)) => merge_motion(previous, next),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cursio_protocol::InputEvent::{Key, PointerMotion};

    #[tokio::test]
    async fn motion_is_bounded_and_buttons_keep_their_position() {
        let (tx, mut rx) = channel("test", 4, 1024, |_| 32, merge_motion);
        for _ in 0..100_000 {
            tx.send(PointerMotion { dx: 1, dy: -1 }).unwrap();
        }
        let key = Key {
            key: 1,
            pressed: true,
        };
        tx.send(key).unwrap();
        tx.send(PointerMotion { dx: 3, dy: 4 }).unwrap();
        assert_eq!(tx.stats().peak_items, 3);
        assert_eq!(tx.stats().merged, 99_999);
        assert_eq!(
            rx.recv().await,
            Some(PointerMotion {
                dx: 100_000,
                dy: -100_000
            })
        );
        assert_eq!(rx.recv().await, Some(key));
        assert_eq!(rx.recv().await, Some(PointerMotion { dx: 3, dy: 4 }));
    }

    #[tokio::test]
    async fn overload_is_explicit_and_never_leaves_a_partially_valid_stream() {
        let (tx, mut rx) = channel("test", 2, 8, |_| 4, |_, _| false);
        tx.send(1).unwrap();
        tx.send(2).unwrap();
        assert!(tx.send(3).is_err());
        assert!(rx.stats().failed);
        assert_eq!(rx.recv().await, None);
        assert_eq!(tx.stats().bytes, 0);
    }

    #[tokio::test]
    async fn closing_drains_accepted_work_and_sender_drop_wakes_the_reader() {
        let (tx, mut rx) = channel("test", 2, 16, |_| 4, |_, _| false);
        tx.send(1).unwrap();
        tx.close();
        assert!(tx.send(2).is_err());
        assert_eq!(rx.recv().await, Some(1));
        assert_eq!(rx.recv().await, None);
        let (tx, mut rx) = channel::<u32>("drop", 1, 4, |_| 4, |_, _| false);
        let pending = tokio::spawn(async move { rx.recv().await });
        tokio::task::yield_now().await;
        drop(tx);
        assert_eq!(pending.await.unwrap(), None);
    }
}
