//! # Rate limiting module.
//!
//! This module contains implementation of token bucket policy.
//! Its primary use is preventing Delta Chat from sending too many messages, especially automatic,
//! such as read receipts.

use std::time::{Duration, SystemTime};

#[derive(Debug)]
pub(crate) struct Ratelimit {
    /// Time of the last update.
    last_update: SystemTime,

    /// Number of messages sent within the time window ending at `last_update`.
    current_value: f64,

    /// Time window size.
    window: Duration,

    /// Number of messages allowed to send within the time window.
    quota: f64,
}

impl Ratelimit {
    /// Returns a new rate limiter with the given constraints.
    ///
    /// Rate limiter will allow to send no more than `quota` messages within duration `window`.
    pub(crate) fn new(window: Duration, quota: f64) -> Self {
        Self::new_at(window, quota, SystemTime::now())
    }

    /// Returns a new rate limiter with given current time for testing purposes.
    const fn new_at(window: Duration, quota: f64, now: SystemTime) -> Self {
        Self {
            last_update: now,
            current_value: 0.0,
            window,
            quota,
        }
    }

    /// Update current value.
    pub(crate) fn update_at(&mut self, now: SystemTime) {
        let rate: f64 = self.quota / self.window.as_secs_f64();
        let elapsed = now
            .duration_since(self.last_update)
            .unwrap_or(Duration::ZERO)
            .as_secs_f64()
            .max(0.0);
        self.current_value = (self.current_value - rate * elapsed).max(0.0);
        self.last_update = now;
    }

    /// Returns true if it is allowed to send a message.
    fn can_send_at(&mut self, now: SystemTime) -> bool {
        self.update_at(now);
        self.current_value <= self.quota
    }

    /// Returns true if can send another message now.
    pub(crate) fn can_send(&mut self) -> bool {
        self.can_send_at(SystemTime::now())
    }

    fn send_at(&mut self, now: SystemTime) {
        self.update_at(now);
        self.current_value += 1.0;
    }

    /// Increases current usage value.
    ///
    /// It is possible to send message even if over quota, e.g. if the message sending is initiated
    /// by the user and should not be rate limited. However, sending messages when over quota
    /// further postpones the time when it will be allowed to send low priority messages.
    pub(crate) fn send(&mut self) {
        self.send_at(SystemTime::now())
    }

    fn until_can_send_at(&mut self, now: SystemTime) -> Duration {
        self.update_at(now);
        if self.current_value <= self.quota {
            Duration::ZERO
        } else {
            let requirement = self.current_value - self.quota;
            let rate = self.quota / self.window.as_secs_f64();
            Duration::from_secs_f64(requirement / rate)
        }
    }

    /// Calculates the time until `can_send` will return `true`.
    pub(crate) fn until_can_send(&mut self) -> Duration {
        self.until_can_send_at(SystemTime::now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ratelimit() {
        let now = SystemTime::now();

        let mut ratelimit = Ratelimit::new(Duration::new(60, 0), 3.0);
        assert!(ratelimit.can_send_at(now));

        // Send burst of 3 messages.
        ratelimit.send_at(now);
        assert!(ratelimit.can_send_at(now));
        ratelimit.send_at(now);
        assert!(ratelimit.can_send_at(now));
        ratelimit.send_at(now);
        assert!(ratelimit.can_send_at(now));
        ratelimit.send_at(now);

        // Can't send more messages now.
        assert!(!ratelimit.can_send_at(now));

        // Can send one more message 20 seconds later.
        assert_eq!(ratelimit.until_can_send_at(now), Duration::from_secs(20));
        let now = now + Duration::from_secs(20);
        assert!(ratelimit.can_send_at(now));
        ratelimit.send_at(now);
        assert!(!ratelimit.can_send_at(now));

        // Send one more message anyway, over quota.
        ratelimit.send_at(now);

        // Waiting 20 seconds is not enough.
        let now = now + Duration::from_secs(20);
        assert!(!ratelimit.can_send_at(now));

        // Can send another message after 40 seconds.
        let now = now + Duration::from_secs(20);
        assert!(ratelimit.can_send_at(now));
    }
}
