use std::time::{Duration, Instant};
use std::convert::TryInto;

/// Token bucket based rate limiter
pub struct RateLimiter {
    rate: u64, // tokens/second
    start_time: Instant,
    traffic: u64, // overall traffic
    bucket_size: u64,
    last_update: Instant,
    consumed_tokens: u64,
}

impl RateLimiter {

    const NO_DELAY: Duration = Duration::from_millis(0);

    /// Creates a new instance, using [Instant::now] as start time.
    pub fn new(rate: u64, bucket_size: u64) -> Self {
        let start_time = Instant::now();
        Self::with_start_time(rate, bucket_size, start_time)
    }

    /// Creates a new instance with specified `rate`, `bucket_size` and `start_time`.
    pub fn with_start_time(rate: u64, bucket_size: u64, start_time: Instant) -> Self {
        Self {
            rate,
            start_time,
            traffic: 0,
            bucket_size,
            last_update: start_time,
            // start with empty bucket (all tokens consumed)
            consumed_tokens: bucket_size,
        }
    }

    /// Returns the average rate (since `start_time`)
    pub fn average_rate(&self, current_time: Instant) -> f64 {
        let time_diff = (current_time - self.start_time).as_secs_f64();
        if time_diff <= 0.0 {
            0.0
        } else {
            (self.traffic as f64) / time_diff
        }
    }

    fn refill_bucket(&mut self, current_time: Instant) {
        let time_diff = (current_time - self.last_update).as_nanos();

        if time_diff <= 0 {
            //log::error!("update_time: got negative time diff");
            return;
        }

        self.last_update = current_time;

        let allowed_traffic = ((time_diff.saturating_mul(self.rate as u128)) / 1_000_000_000)
            .try_into().unwrap_or(u64::MAX);

        self.consumed_tokens = self.consumed_tokens.saturating_sub(allowed_traffic);
    }

    /// Register traffic, returning a proposed delay to reach the expected rate.
    pub fn register_traffic(&mut self, current_time: Instant, data_len: u64) -> Duration {
        self.refill_bucket(current_time);

        self.traffic += data_len;
        self.consumed_tokens += data_len;

        if self.consumed_tokens <= self.bucket_size {
            return Self::NO_DELAY;
        }
        Duration::from_nanos((self.consumed_tokens - self.bucket_size).saturating_mul(1_000_000_000)/ self.rate)
    }
}
