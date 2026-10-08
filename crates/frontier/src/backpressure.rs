//! Bounded channels for the fetch -> parse -> index -> graph pipeline
//! (spec §5.5). Every channel is bounded; a slower downstream stage applies
//! backpressure upstream instead of buffering without limit.

use tokio::sync::mpsc;

/// Create a bounded channel. When the buffer is full, `send().await` pends
/// rather than dropping, so the upstream dispatcher idles — never loses work.
pub fn bounded<T>(capacity: usize) -> (mpsc::Sender<T>, mpsc::Receiver<T>) {
    assert!(capacity > 0, "backpressure channel capacity must be > 0");
    mpsc::channel(capacity)
}

/// A single pipeline stage's bounded input/output pair, named for clarity at
/// call sites (`let parse = Stage::new(...)`).
pub struct Stage<T> {
    pub tx: mpsc::Sender<T>,
    pub rx: mpsc::Receiver<T>,
}

impl<T> Stage<T> {
    pub fn new(capacity: usize) -> Self {
        let (tx, rx) = bounded(capacity);
        Self { tx, rx }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bounded_channel_enforces_capacity() {
        let (tx, mut rx) = bounded::<u32>(2);
        tx.send(1).await.unwrap();
        tx.send(2).await.unwrap();
        assert_eq!(tx.max_capacity(), 2);
        assert_eq!(tx.capacity(), 0, "buffer full");
        assert_eq!(rx.recv().await, Some(1));
        // A freed slot unblocks a third send.
        tx.send(3).await.unwrap();
        assert_eq!(rx.recv().await, Some(2));
        assert_eq!(rx.recv().await, Some(3));
    }

    #[tokio::test]
    async fn send_pends_when_full_rather_than_dropping() {
        let (tx, _rx) = bounded::<u32>(1);
        tx.send(1).await.unwrap();
        // No receiver draining: a second send must not complete immediately.
        let res = tokio::time::timeout(std::time::Duration::from_millis(50), tx.send(2)).await;
        assert!(res.is_err(), "send should have applied backpressure");
    }
}
