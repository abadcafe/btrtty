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
    retired_channel: Option<u64>,
}

impl<T> IncomingListener<T> {
    pub(super) fn new(channel_id: u8, notification: T, registered_at: Instant) -> Self {
        Self {
            channel_id,
            generations: VecDeque::from([Generation {
                registered_at,
                notification,
            }]),
            retired_channel: None,
        }
    }

    /// Start a fresh native registration epoch. Clearing the retirement fence
    /// before calling `IOBluetooth` permits a synchronously delivered pending
    /// connection to use the same native object ID in the new epoch.
    pub(super) fn begin_registration(&mut self) -> u8 {
        self.retired_channel = None;
        self.channel_id
    }

    pub(super) fn accepts_channel(&self, channel_key: u64) -> bool {
        self.retired_channel != Some(channel_key)
    }

    /// Stop every matching incoming notification while one client owns the
    /// server. The retired channel key fences callbacks from those generations
    /// until `begin_registration` starts a genuinely new listener epoch.
    pub(super) fn suspend_for_channel(&mut self, channel_key: u64) -> Vec<T> {
        self.retired_channel = Some(channel_key);
        self.generations
            .drain(..)
            .map(|generation| generation.notification)
            .collect()
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
