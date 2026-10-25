//! Keep transport backpressure outside the cursor/ownership loop.
use crate::{module_host::LinkTx, queue};
use cursio_protocol::Message;
use cursio_transport::{TransportError, TransportTx};
use std::time::Duration;

pub(crate) struct SessionOutbox {
    sender: queue::Sender<Message>,
    task: tokio::task::JoinHandle<Result<(), TransportError>>,
    closer: LinkTx,
    completed: bool,
}

impl SessionOutbox {
    pub(crate) fn new(mut tx: LinkTx) -> Self {
        let closer = tx.closer();
        let (sender, mut pending) = queue::channel(
            "session outbound",
            512,
            1024 * 1024,
            queue::message_bytes,
            queue::merge_messages,
        );
        let task = tokio::spawn(async move {
            while let Some(message) = pending.recv().await {
                tx.send(&message).await?;
            }
            tx.flush().await;
            Ok(())
        });
        Self {
            sender,
            task,
            closer,
            completed: false,
        }
    }

    pub(crate) async fn finished(&mut self) -> Result<(), TransportError> {
        let result = (&mut self.task).await;
        self.completed = true;
        match result {
            Ok(result) => result,
            Err(error) => Err(TransportError::Other(format!(
                "session sender stopped: {error}"
            ))),
        }
    }

    pub(crate) async fn finish(&mut self, deadline: Duration) {
        self.sender.close();
        if !self.completed
            && tokio::time::timeout(deadline, &mut self.task)
                .await
                .is_err()
        {
            tracing::warn!(
                ?deadline,
                "session sender did not drain before shutdown deadline"
            );
            self.task.abort();
        }
        self.closer.close().await;
    }
}

#[async_trait::async_trait]
impl TransportTx for SessionOutbox {
    async fn send(&mut self, message: &Message) -> Result<(), TransportError> {
        self.sender.send(message.clone()).map_err(|_| {
            TransportError::Other("session outbound queue closed or overloaded".into())
        })
    }
}

impl Drop for SessionOutbox {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Blocked;
    #[async_trait::async_trait]
    impl TransportTx for Blocked {
        async fn send(&mut self, _: &Message) -> Result<(), TransportError> {
            std::future::pending().await
        }
    }
    #[tokio::test(start_paused = true)]
    async fn blocked_network_does_not_block_admission_or_shutdown() {
        let mut outbox = SessionOutbox::new(LinkTx::new(Box::new(Blocked)));
        outbox.send(&Message::Heartbeat { nonce: 1 }).await.unwrap();
        tokio::task::yield_now().await;
        outbox.send(&Message::Grab { active: true }).await.unwrap();
        let start = tokio::time::Instant::now();
        outbox.finish(Duration::from_millis(100)).await;
        assert!(start.elapsed() <= Duration::from_millis(101));
    }
}
