use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

struct Bucket {
    tokens: f64,
    updated: Instant,
    connections: usize,
}

pub struct IpLimits {
    rate: f64,
    burst: f64,
    max_connections: usize,
    buckets: Mutex<HashMap<IpAddr, Bucket>>,
}

impl IpLimits {
    pub fn new(rate: u32, burst: u32, max_connections: usize) -> Self {
        Self {
            rate: f64::from(rate),
            burst: f64::from(burst),
            max_connections,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<IpAddr, Bucket>> {
        self.buckets.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn connect(&self, ip: IpAddr) -> bool {
        let mut buckets = self.lock();
        let bucket = buckets.entry(ip).or_insert_with(|| Bucket {
            tokens: self.burst,
            updated: Instant::now(),
            connections: 0,
        });
        if bucket.connections >= self.max_connections {
            return false;
        }
        bucket.connections += 1;
        true
    }

    pub fn disconnect(&self, ip: IpAddr) {
        let mut buckets = self.lock();
        if let Some(bucket) = buckets.get_mut(&ip) {
            bucket.connections = bucket.connections.saturating_sub(1);
            if bucket.connections == 0 {
                buckets.remove(&ip);
            }
        }
    }

    /// Takes a message's token from the IP's bucket and says how long to
    /// wait before handling the message: zero while the bucket has tokens,
    /// and once it is empty, until it earned back the tokens it owes. Senders
    /// over the rate are slowed down this way instead of being cut off.
    pub fn delay_message(&self, ip: IpAddr) -> Duration {
        let mut buckets = self.lock();
        let Some(bucket) = buckets.get_mut(&ip) else {
            return Duration::ZERO;
        };
        let now = Instant::now();
        let elapsed = now.duration_since(bucket.updated).as_secs_f64();
        bucket.updated = now;
        bucket.tokens = (bucket.tokens + elapsed * self.rate).min(self.burst) - 1.0;
        if bucket.tokens >= 0.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(-bucket.tokens / self.rate)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_connections_and_bursts() {
        let limits = IpLimits::new(1, 3, 2);
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        assert!(limits.connect(ip));
        assert!(limits.connect(ip));
        assert!(!limits.connect(ip));
        assert!((0..3).all(|_| limits.delay_message(ip).is_zero()));
        // Over the burst, each message waits for its token: about a second
        // at one message a second, then two for the one after it.
        let first = limits.delay_message(ip);
        assert!(first > Duration::from_millis(900) && first <= Duration::from_secs(1));
        let second = limits.delay_message(ip);
        assert!(second > Duration::from_millis(1900) && second <= Duration::from_secs(2));
        limits.disconnect(ip);
        assert!(limits.connect(ip));
        limits.disconnect(ip);
        limits.disconnect(ip);
        assert!(limits.delay_message(ip).is_zero());
    }
}
