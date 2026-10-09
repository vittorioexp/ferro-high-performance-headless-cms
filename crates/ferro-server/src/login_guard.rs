//! Brute-force protection for `POST /auth/login`.
//!
//! After `max` failed attempts for an email (existing or not: unknown emails count too, so the
//! lock does not reveal which accounts exist) the email is locked for `lockout`; the same
//! applies per client address with a limit ten times higher. A success clears the email counter.
//! State is per instance (in memory): behind a load balancer the effective limit is `max` per instance.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

struct Entry {
    failures: u32,
    last: Instant,
}

pub struct LoginGuard {
    entries: Mutex<HashMap<String, Entry>>,
    max: u32,
    lockout: Duration,
}

const IP_FACTOR: u32 = 10;
const PRUNE_ABOVE: usize = 10_000;

impl LoginGuard {
    pub fn new(max: u32, lockout: Duration) -> LoginGuard {
        LoginGuard { entries: Mutex::new(HashMap::new()), max: max.max(1), lockout }
    }

    fn keys(email: &str, ip: &str) -> [(String, bool); 2] {
        [(format!("email:{}", email.trim().to_lowercase()), false), (format!("ip:{ip}"), true)]
    }

    fn limit(&self, is_ip: bool) -> u32 {
        if is_ip {
            self.max.saturating_mul(IP_FACTOR)
        } else {
            self.max
        }
    }

    /// `Err(seconds)` while the email or the address is locked.
    pub fn check(&self, email: &str, ip: &str) -> Result<(), u64> {
        let map = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        for (key, is_ip) in Self::keys(email, ip) {
            if let Some(e) = map.get(&key) {
                let age = now.duration_since(e.last);
                if e.failures >= self.limit(is_ip) && age < self.lockout {
                    return Err((self.lockout - age).as_secs().max(1));
                }
            }
        }
        Ok(())
    }

    pub fn failure(&self, email: &str, ip: &str) {
        let mut map = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        if map.len() > PRUNE_ABOVE {
            let lockout = self.lockout;
            map.retain(|_, e| now.duration_since(e.last) < lockout);
        }
        for (key, _) in Self::keys(email, ip) {
            let e = map.entry(key).or_insert(Entry { failures: 0, last: now });
            if now.duration_since(e.last) >= self.lockout {
                e.failures = 0; // the previous burst is over
            }
            e.failures += 1;
            e.last = now;
        }
    }

    pub fn success(&self, email: &str) {
        let mut map = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        map.remove(&format!("email:{}", email.trim().to_lowercase()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locks_after_max_failures_and_unlocks() {
        let g = LoginGuard::new(3, Duration::from_millis(80));
        for _ in 0..2 {
            g.failure("A@x.com", "1.1.1.1");
            assert!(g.check("a@x.com", "1.1.1.1").is_ok());
        }
        g.failure("a@x.com", "1.1.1.1");
        assert!(g.check("A@X.com", "9.9.9.9").is_err(), "the email is locked whatever the address (case-insensitive)");
        assert!(g.check("other@x.com", "1.1.1.1").is_ok(), "other accounts are not affected");
        std::thread::sleep(Duration::from_millis(100));
        assert!(g.check("a@x.com", "1.1.1.1").is_ok(), "unlocked after the lockout");
        g.failure("a@x.com", "1.1.1.1");
        assert!(g.check("a@x.com", "1.1.1.1").is_ok(), "the counter restarted");
    }

    #[test]
    fn success_clears_the_email_counter() {
        let g = LoginGuard::new(2, Duration::from_secs(60));
        g.failure("a@x.com", "1.1.1.1");
        g.success("a@x.com");
        g.failure("a@x.com", "1.1.1.1");
        assert!(g.check("a@x.com", "1.1.1.1").is_ok());
    }

    #[test]
    fn one_address_guessing_many_emails_is_locked_too() {
        let g = LoginGuard::new(2, Duration::from_secs(60)); // address limit = 20
        for i in 0..20 {
            assert!(g.check(&format!("u{i}@x.com"), "6.6.6.6").is_ok());
            g.failure(&format!("u{i}@x.com"), "6.6.6.6");
        }
        assert!(g.check("fresh@x.com", "6.6.6.6").is_err());
        assert!(g.check("fresh@x.com", "7.7.7.7").is_ok());
    }
}
