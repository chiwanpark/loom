use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use loom_proto::relay::AgentId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LimitKey {
    Ip(IpAddr),
    Peer([u8; 32]),
}

impl LimitKey {
    pub fn agent(agent: &AgentId) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"loom-relay-limit-agent/1");
        hasher.update(&agent.host.0);
        hasher.update(agent.instance.0.as_bytes());
        Self::Peer(*hasher.finalize().as_bytes())
    }

    pub fn client(pubkey: &[u8; 32]) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"loom-relay-limit-client/1");
        hasher.update(pubkey);
        Self::Peer(*hasher.finalize().as_bytes())
    }
}

struct Bucket {
    tokens: f64,
    updated: Instant,
    connections: usize,
}

pub struct Limits {
    rate: f64,
    burst: f64,
    max_connections: usize,
    buckets: Mutex<HashMap<LimitKey, Bucket>>,
}

impl Limits {
    pub fn new(rate: u32, burst: u32, max_connections: usize) -> Self {
        Self {
            rate: f64::from(rate),
            burst: f64::from(burst),
            max_connections,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<LimitKey, Bucket>> {
        self.buckets.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn bucket(&self) -> Bucket {
        Bucket {
            tokens: self.burst,
            updated: Instant::now(),
            connections: 0,
        }
    }

    pub fn connect(&self, key: LimitKey) -> bool {
        let mut buckets = self.lock();
        let bucket = buckets.entry(key).or_insert_with(|| self.bucket());
        if bucket.connections >= self.max_connections {
            return false;
        }
        bucket.connections += 1;
        true
    }

    pub fn disconnect(&self, key: LimitKey) {
        let mut buckets = self.lock();
        release(&mut buckets, key);
    }

    pub fn rekey(&self, from: LimitKey, to: LimitKey) -> bool {
        if from == to {
            return true;
        }
        let mut buckets = self.lock();
        let bucket = buckets.entry(to).or_insert_with(|| self.bucket());
        if bucket.connections >= self.max_connections {
            if bucket.connections == 0 {
                buckets.remove(&to);
            }
            return false;
        }
        bucket.connections += 1;
        release(&mut buckets, from);
        true
    }

    /// Takes a message's token from the bucket and says how long to wait
    /// before handling the message: zero while the bucket has tokens, and
    /// once it is empty, until it earned back the tokens it owes. Senders
    /// over the rate are slowed down this way instead of being cut off.
    pub fn delay_message(&self, key: LimitKey) -> Duration {
        let mut buckets = self.lock();
        let Some(bucket) = buckets.get_mut(&key) else {
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

    #[cfg(test)]
    fn connections(&self, key: LimitKey) -> usize {
        self.lock().get(&key).map_or(0, |b| b.connections)
    }
}

fn release(buckets: &mut HashMap<LimitKey, Bucket>, key: LimitKey) {
    if let Some(bucket) = buckets.get_mut(&key) {
        bucket.connections = bucket.connections.saturating_sub(1);
        if bucket.connections == 0 {
            buckets.remove(&key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use loom_proto::InstanceId;
    use loom_proto::relay::HostId;

    #[test]
    fn limits_connections_and_bursts() {
        let limits = Limits::new(1, 3, 2);
        let ip = LimitKey::Ip("10.0.0.1".parse().unwrap());
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

    #[test]
    fn peers_leave_the_bucket_of_their_address() {
        let limits = Limits::new(1, 2, 2);
        let ip = LimitKey::Ip("10.0.0.1".parse().unwrap());
        let agent = |name: &str| {
            LimitKey::agent(&AgentId {
                host: HostId([1; 32]),
                instance: InstanceId(name.into()),
            })
        };
        assert_ne!(agent("a"), agent("b"));
        assert_ne!(LimitKey::client(&[1; 32]), LimitKey::client(&[2; 32]));
        assert!(limits.connect(ip));
        assert!(limits.connect(ip));
        assert!(!limits.connect(ip));
        assert!(limits.rekey(ip, agent("a")));
        assert!(limits.connect(ip));
        assert!(limits.rekey(ip, agent("b")));
        assert!(limits.rekey(ip, LimitKey::client(&[1; 32])));
        assert_eq!(limits.connections(ip), 0);
        assert_eq!(limits.connections(agent("a")), 1);

        assert!(limits.connect(ip));
        assert!((0..2).all(|_| limits.delay_message(ip).is_zero()));
        assert!(!limits.delay_message(ip).is_zero());
        assert!(limits.delay_message(agent("a")).is_zero());

        assert!(limits.rekey(ip, agent("a")));
        assert!(limits.connect(ip));
        assert!(!limits.rekey(ip, agent("a")));
        assert_eq!(limits.connections(ip), 1);
        assert!(limits.rekey(agent("a"), agent("a")));
        limits.disconnect(agent("a"));
        limits.disconnect(agent("a"));
        assert_eq!(limits.connections(agent("a")), 0);
    }
}
