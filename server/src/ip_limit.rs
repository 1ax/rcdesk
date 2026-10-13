//! Per-IP token-bucket rate limits (debt D36, slice 3.6a; extended in B2).
//!
//! Three buckets live in `AppState`, each an `IpLimiter` with its own burst
//! and refill (constants in `app.rs`): wrong PIN guesses (3.6a), creation of
//! new anonymous owners and registration of new devices (B2, audit S-H2).
//! Only the *event being limited* spends a token (an unknown PIN, a freshly
//! created owner, a freshly issued device); a successful `Join`, a known
//! token or a known device is free. Same style as `Registry`: a std mutex
//! held for short, non-async sections.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Above this many tracked IPs, fully recovered buckets are dropped on write.
const GC_THRESHOLD: usize = 10_000;

struct Bucket {
    tokens: f64,
    updated: Instant,
}

impl Bucket {
    /// Tokens available at `now` (refill applied, capped at the burst).
    fn tokens_at(&self, now: Instant, burst: u32, refill: Duration) -> f64 {
        let elapsed = now.saturating_duration_since(self.updated);
        let refilled = elapsed.as_secs_f64() / refill.as_secs_f64();
        (self.tokens + refilled).min(burst as f64)
    }
}

#[derive(Clone)]
pub struct IpLimiter {
    /// Tokens a fresh (or fully recovered) IP has.
    burst: u32,
    /// One token comes back this often.
    refill: Duration,
    buckets: Arc<Mutex<HashMap<IpAddr, Bucket>>>,
}

impl IpLimiter {
    pub fn new(burst: u32, refill: Duration) -> Self {
        Self {
            burst,
            refill,
            buckets: Arc::default(),
        }
    }

    /// Whether `ip` still has a token at `now`. Does not spend it.
    pub fn check_at(&self, ip: IpAddr, now: Instant) -> bool {
        let buckets = self.buckets.lock().expect("ip limiter mutex poisoned");
        match buckets.get(&ip) {
            Some(bucket) => bucket.tokens_at(now, self.burst, self.refill) >= 1.0,
            None => true,
        }
    }

    /// Spends one token of `ip` for one limited event.
    pub fn spend_at(&self, ip: IpAddr, now: Instant) {
        let mut buckets = self.buckets.lock().expect("ip limiter mutex poisoned");
        let (burst, refill) = (self.burst, self.refill);
        if buckets.len() > GC_THRESHOLD {
            buckets.retain(|_, bucket| bucket.tokens_at(now, burst, refill) < burst as f64);
        }
        let bucket = buckets.entry(ip).or_insert(Bucket {
            tokens: burst as f64,
            updated: now,
        });
        bucket.tokens = (bucket.tokens_at(now, burst, refill) - 1.0).max(0.0);
        bucket.updated = now;
    }

    pub fn check(&self, ip: IpAddr) -> bool {
        self.check_at(ip, Instant::now())
    }

    pub fn spend(&self, ip: IpAddr) {
        self.spend_at(ip, Instant::now());
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.buckets.lock().expect("mutex").len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    const PIN_BURST: u32 = 10;
    const PIN_REFILL: Duration = Duration::from_secs(60);

    fn pin_limiter() -> IpLimiter {
        IpLimiter::new(PIN_BURST, PIN_REFILL)
    }

    fn ip(last: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(10, 0, 0, last))
    }

    #[test]
    fn ten_failures_pass_then_blocked() {
        let limiter = pin_limiter();
        let now = Instant::now();
        for _ in 0..PIN_BURST {
            assert!(limiter.check_at(ip(1), now));
            limiter.spend_at(ip(1), now);
        }
        assert!(!limiter.check_at(ip(1), now));
    }

    #[test]
    fn check_does_not_spend_tokens() {
        let limiter = pin_limiter();
        let now = Instant::now();
        for _ in 0..100 {
            assert!(limiter.check_at(ip(1), now));
        }
    }

    #[test]
    fn one_attempt_returns_after_refill_interval() {
        let limiter = pin_limiter();
        let now = Instant::now();
        for _ in 0..PIN_BURST {
            limiter.spend_at(ip(1), now);
        }
        let later = now + PIN_REFILL;
        assert!(limiter.check_at(ip(1), later));
        limiter.spend_at(ip(1), later);
        assert!(!limiter.check_at(ip(1), later));
    }

    #[test]
    fn ips_are_independent() {
        let limiter = pin_limiter();
        let now = Instant::now();
        for _ in 0..PIN_BURST {
            limiter.spend_at(ip(1), now);
        }
        assert!(!limiter.check_at(ip(1), now));
        assert!(limiter.check_at(ip(2), now));
    }

    #[test]
    fn gc_keeps_partial_buckets_and_drops_recovered() {
        let limiter = pin_limiter();
        let now = Instant::now();
        // One recovered bucket (single failure, long ago) and one partial.
        limiter.spend_at(ip(1), now);
        let later = now + PIN_REFILL * 5;
        limiter.spend_at(ip(2), later);
        // Fill the map past the threshold with throwaway recovered entries.
        {
            let mut buckets = limiter.buckets.lock().unwrap();
            for n in 0..(GC_THRESHOLD as u32 + 1) {
                buckets.insert(
                    IpAddr::V4(Ipv4Addr::from(0x0100_0000 + n)),
                    Bucket {
                        tokens: PIN_BURST as f64,
                        updated: now,
                    },
                );
            }
        }
        limiter.spend_at(ip(3), later);
        // ip(1) recovered fully, ip(2) and ip(3) are partial.
        assert_eq!(limiter.len(), 2);
        let buckets = limiter.buckets.lock().unwrap();
        assert!(!buckets.contains_key(&ip(1)));
        assert!(buckets.contains_key(&ip(2)));
        assert!(buckets.contains_key(&ip(3)));
    }

    #[test]
    fn custom_burst_is_respected() {
        let limiter = IpLimiter::new(2, Duration::from_secs(60));
        let now = Instant::now();
        limiter.spend_at(ip(1), now);
        assert!(limiter.check_at(ip(1), now));
        limiter.spend_at(ip(1), now);
        assert!(!limiter.check_at(ip(1), now));
    }
}
