//! Per-user failure lockout in front of any validator.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use ironrdp_server::{CredentialDecision, CredentialValidationError, CredentialValidator, Credentials};
use tracing::warn;

use crate::bare_username;

struct Entry {
    failures: u32,
    first_failure: Instant,
    locked_until: Option<Instant>,
}

/// Rejects a username outright while it is locked, and locks it after `max_failures` rejections
/// within `window`. Accepts clear the record.
pub struct Lockout<V> {
    inner: V,
    max_failures: u32,
    window: Duration,
    lock_for: Duration,
    entries: Mutex<HashMap<String, Entry>>,
}

impl<V> Lockout<V> {
    pub fn new(inner: V) -> Self {
        Self::with_policy(inner, 5, Duration::from_secs(300), Duration::from_secs(300))
    }

    pub fn with_policy(inner: V, max_failures: u32, window: Duration, lock_for: Duration) -> Self {
        Self {
            inner,
            max_failures,
            window,
            lock_for,
            entries: Mutex::new(HashMap::new()),
        }
    }

    fn is_locked(&self, user: &str, now: Instant) -> bool {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        match entries.get_mut(user) {
            Some(entry) => match entry.locked_until {
                Some(until) if now < until => true,
                Some(_) => {
                    entries.remove(user);
                    false
                }
                None => false,
            },
            None => false,
        }
    }

    fn record(&self, user: &str, accepted: bool, now: Instant) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        if accepted {
            entries.remove(user);
            return;
        }
        let entry = entries.entry(user.to_owned()).or_insert(Entry {
            failures: 0,
            first_failure: now,
            locked_until: None,
        });
        if now.duration_since(entry.first_failure) > self.window {
            entry.failures = 0;
            entry.first_failure = now;
        }
        entry.failures += 1;
        if entry.failures >= self.max_failures {
            entry.locked_until = Some(now + self.lock_for);
            warn!(user, seconds = self.lock_for.as_secs(), "too many failed logins, user locked");
        }
    }
}

#[async_trait]
impl<V: CredentialValidator> CredentialValidator for Lockout<V> {
    async fn validate(&self, credentials: &Credentials) -> Result<CredentialDecision, CredentialValidationError> {
        let user = bare_username(&credentials.username).to_owned();
        let now = Instant::now();
        if self.is_locked(&user, now) {
            warn!(user, "login attempt while locked out");
            return Ok(CredentialDecision::Reject);
        }
        let decision = self.inner.validate(credentials).await?;
        self.record(&user, decision == CredentialDecision::Accept, now);
        Ok(decision)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct AlwaysReject;

    #[async_trait]
    impl CredentialValidator for AlwaysReject {
        async fn validate(&self, _: &Credentials) -> Result<CredentialDecision, CredentialValidationError> {
            Ok(CredentialDecision::Reject)
        }
    }

    #[tokio::test]
    async fn locks_after_repeated_failures() {
        let v = Lockout::with_policy(AlwaysReject, 2, Duration::from_secs(60), Duration::from_secs(60));
        let creds = Credentials {
            username: "bob".into(),
            password: "x".into(),
            domain: None,
        };
        for _ in 0..3 {
            assert_eq!(v.validate(&creds).await.unwrap(), CredentialDecision::Reject);
        }
        assert!(v.is_locked("bob", Instant::now()));
    }
}
