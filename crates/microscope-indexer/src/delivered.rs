use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use solana_signature::Signature;
use tokio::sync::mpsc::{
    error::TryRecvError, unbounded_channel, UnboundedReceiver, UnboundedSender,
};
use tokio::sync::Mutex;

/// Hands the signatures Yellowstone delivered to the RPC poller, which records
/// them in its durable checkpoint so a poll stalled past the pipeline's
/// deduplication window does not re-emit them.
#[derive(Clone, Debug)]
pub struct DeliveredSignatures {
    sender: UnboundedSender<(Signature, u64)>,
    receiver: Arc<Mutex<UnboundedReceiver<(Signature, u64)>>>,
    recording: Arc<AtomicBool>,
}

impl DeliveredSignatures {
    pub fn new() -> Self {
        let (sender, receiver) = unbounded_channel();
        Self {
            sender,
            receiver: Arc::new(Mutex::new(receiver)),
            recording: Arc::new(AtomicBool::new(true)),
        }
    }

    pub fn record(&self, signature: Signature, slot: u64) {
        if !self.recording.load(Ordering::Relaxed) {
            return;
        }
        let _ = self.sender.send((signature, slot));
    }

    pub(crate) async fn drain(&self) -> Vec<(Signature, u64)> {
        let mut receiver = self.receiver.lock().await;
        let mut drained = Vec::new();
        loop {
            match receiver.try_recv() {
                Ok(delivery) => drained.push(delivery),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return drained,
            }
        }
    }

    /// A parked poller emits nothing, so nothing can be duplicated and the
    /// undrained deliveries would only grow.
    pub(crate) async fn stop(&self) {
        self.recording.store(false, Ordering::Relaxed);
        self.drain().await;
    }
}

impl Default for DeliveredSignatures {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::DeliveredSignatures;

    fn signature(value: u64) -> solana_signature::Signature {
        let mut bytes = [0; 64];
        bytes[..8].copy_from_slice(&value.to_le_bytes());
        solana_signature::Signature::from(bytes)
    }

    #[tokio::test]
    async fn drains_every_recorded_delivery_once() {
        let delivered = DeliveredSignatures::new();
        delivered.record(signature(1), 100);
        delivered.record(signature(2), 101);

        assert_eq!(
            delivered.drain().await,
            vec![(signature(1), 100), (signature(2), 101)]
        );
        assert!(delivered.drain().await.is_empty());
    }

    #[tokio::test]
    async fn stops_recording_once_the_poller_parks() {
        let delivered = DeliveredSignatures::new();
        delivered.record(signature(1), 100);

        delivered.stop().await;
        delivered.record(signature(2), 101);

        assert!(delivered.drain().await.is_empty());
    }
}
