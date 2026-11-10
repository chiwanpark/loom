use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::Instant;

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

    pub fn allow_message(&self, ip: IpAddr) -> bool {
        let mut buckets = self.lock();
        let Some(bucket) = buckets.get_mut(&ip) else {
            return true;
        };
        let now = Instant::now();
        let elapsed = now.duration_since(bucket.updated).as_secs_f64();
        bucket.updated = now;
        bucket.tokens = (bucket.tokens + elapsed * self.rate).min(self.burst);
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
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
        assert!((0..3).all(|_| limits.allow_message(ip)));
        assert!(!limits.allow_message(ip));
        limits.disconnect(ip);
        assert!(limits.connect(ip));
        limits.disconnect(ip);
        limits.disconnect(ip);
        assert!(limits.allow_message(ip));
    }
}
