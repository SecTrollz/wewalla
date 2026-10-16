//! Fan-out of serialized frames to streaming clients. A slow client loses frames; it
//! never blocks the DSP loop or other clients.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};

pub const MAX_SUBSCRIBERS: usize = 64;

/// A new subscription: its receiver plus the most recent frame, if any.
pub type Subscription = (Receiver<Arc<str>>, Option<Arc<str>>);

#[derive(Debug, Default)]
pub struct Hub {
    subs: Mutex<Vec<SyncSender<Arc<str>>>>,
    latest: Mutex<Option<Arc<str>>>,
    pub dropped: AtomicU64,
}

impl Hub {
    pub fn subscribe(&self) -> Option<Subscription> {
        let mut subs = self.subs.lock().ok()?;
        // Dead subscribers are reaped by `publish`, which runs every report hop.
        if subs.len() >= MAX_SUBSCRIBERS {
            return None;
        }
        let (tx, rx) = sync_channel(8);
        subs.push(tx);
        Some((rx, self.latest.lock().ok()?.clone()))
    }

    pub fn publish(&self, msg: Arc<str>) {
        if let Ok(mut l) = self.latest.lock() {
            *l = Some(Arc::clone(&msg));
        }
        if let Ok(mut subs) = self.subs.lock() {
            subs.retain(|s| match s.try_send(Arc::clone(&msg)) {
                Ok(()) => true,
                Err(TrySendError::Full(_)) => {
                    self.dropped.fetch_add(1, Ordering::Relaxed);
                    true
                }
                Err(TrySendError::Disconnected(_)) => false,
            });
        }
    }

    pub fn latest(&self) -> Option<Arc<str>> {
        self.latest.lock().ok()?.clone()
    }

    pub fn subscriber_count(&self) -> usize {
        self.subs.lock().map_or(0, |s| s.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivers_to_all_and_caches_latest() {
        let h = Hub::default();
        h.publish(Arc::from("first"));
        let (a, latest) = h.subscribe().unwrap();
        assert_eq!(latest.as_deref(), Some("first"));
        let (b, _) = h.subscribe().unwrap();
        h.publish(Arc::from("second"));
        let drain = |r: &Receiver<Arc<str>>| {
            r.try_iter()
                .filter(|m| !m.is_empty())
                .map(|m| m.to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(drain(&a), ["second"]);
        assert_eq!(drain(&b), ["second"]);
    }

    #[test]
    fn slow_subscriber_drops_frames_without_blocking() {
        let h = Hub::default();
        let (_rx, _) = h.subscribe().unwrap();
        let t = std::time::Instant::now();
        for i in 0..1000 {
            h.publish(Arc::from(format!("m{i}")));
        }
        assert!(t.elapsed() < std::time::Duration::from_millis(500));
        assert!(h.dropped.load(Ordering::Relaxed) > 900);
        assert_eq!(h.subscriber_count(), 1);
    }

    #[test]
    fn dead_subscribers_are_reaped_and_cap_is_enforced() {
        let h = Hub::default();
        let (rx, _) = h.subscribe().unwrap();
        drop(rx);
        h.publish(Arc::from("x"));
        assert_eq!(h.subscriber_count(), 0);
        let keep: Vec<_> = (0..MAX_SUBSCRIBERS)
            .map(|_| h.subscribe().unwrap())
            .collect();
        assert!(h.subscribe().is_none());
        drop(keep);
    }
}
