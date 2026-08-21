use std::collections::VecDeque;
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2_io_bluetooth::IOBluetoothUserNotification;

/// Current macOS releases discard an idle RFCOMM accept session even though
/// the public notification object remains registered. Retry only while the
/// application is actually waiting for a client.
pub(super) const REFRESH_INTERVAL: Duration = Duration::from_millis(500);

/// A newly registered notification may already own an in-flight native open.
/// Keep recent generations registered long enough for that callback to arrive.
pub(super) const CALLBACK_GRACE: Duration = Duration::from_secs(5);

struct Generation<T> {
    registered_at: Instant,
    notification: T,
}

pub(super) struct IncomingListener<T = Retained<IOBluetoothUserNotification>> {
    channel_id: u8,
    generations: VecDeque<Generation<T>>,
}

impl<T> IncomingListener<T> {
    pub(super) fn new(channel_id: u8, notification: T, registered_at: Instant) -> Self {
        Self {
            channel_id,
            generations: VecDeque::from([Generation {
                registered_at,
                notification,
            }]),
        }
    }

    pub(super) const fn channel_id(&self) -> u8 {
        self.channel_id
    }

    /// Install a fresh native accept generation and return generations whose
    /// callback grace period has elapsed. The newest generation is never
    /// retired by the same operation that installs it.
    pub(super) fn install(&mut self, notification: T, registered_at: Instant) -> Vec<T> {
        self.generations.push_back(Generation {
            registered_at,
            notification,
        });

        let mut retired = Vec::new();
        while self.generations.len() > 1
            && self.generations.front().is_some_and(|generation| {
                registered_at.saturating_duration_since(generation.registered_at) >= CALLBACK_GRACE
            })
        {
            let generation = self
                .generations
                .pop_front()
                .expect("the listener generation was checked");
            retired.push(generation.notification);
        }
        retired
    }

    pub(super) fn into_notifications(self) -> impl Iterator<Item = T> {
        self.generations
            .into_iter()
            .map(|generation| generation.notification)
    }
}
