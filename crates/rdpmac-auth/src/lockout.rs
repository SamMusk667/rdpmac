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
    /// An NLA logon that [`Lockout::attempt`] counted and whose password has not reached the
    /// validator yet.
    pending: bool,
}

/// Rejects a username outright while it is locked, and locks it after `max_failures` rejections
/// within `window`. Accepts clear the record. Usernames count without their domain and case, so
/// spelling one differently does not buy more tries.
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
            pending: false,
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

    /// Starts a logon the validator only sees if the client gets past a check of its own first:
    /// with NLA, NTLM checks the client before it hands over the password. Returns whether `user`
    /// may try now. The try counts as a failure unless the validator accepts the password.
    pub fn attempt(&self, user: &str) -> bool {
        let user = key(user);
        let now = Instant::now();
        if self.is_locked(&user, now) {
            warn!(user, "NLA logon attempt while locked out");
            return false;
        }
        self.record(&user, false, now);
        if let Some(entry) = self.entries.lock().unwrap_or_else(|e| e.into_inner()).get_mut(&user) {
            entry.pending = true;
        }
        true
    }

    /// Whether an [`attempt`](Self::attempt) already counted this logon, which then is no longer
    /// pending.
    fn take_pending(&self, user: &str) -> bool {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries.get_mut(user).is_some_and(|entry| std::mem::take(&mut entry.pending))
    }
}

fn key(username: &str) -> String {
    bare_username(username).to_lowercase()
}

#[async_trait]
impl<V: CredentialValidator> CredentialValidator for Lockout<V> {
    async fn validate(&self, credentials: &Credentials) -> Result<CredentialDecision, CredentialValidationError> {
        let user = key(&credentials.username);
        let now = Instant::now();
        // An NLA logon checked the lock and counted itself before NTLM.
        let counted = self.take_pending(&user);
        if !counted && self.is_locked(&user, now) {
            warn!(user, "login attempt while locked out");
            return Ok(CredentialDecision::Reject);
        }
        let decision = self.inner.validate(credentials).await?;
        let accepted = decision == CredentialDecision::Accept;
        if accepted || !counted {
            self.record(&user, accepted, now);
        }
        Ok(decision)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct AlwaysReject;

    struct AlwaysAccept;

    #[async_trait]
    impl CredentialValidator for AlwaysAccept {
        async fn validate(&self, _: &Credentials) -> Result<CredentialDecision, CredentialValidationError> {
            Ok(CredentialDecision::Accept)
        }
    }

    fn creds(username: &str) -> Credentials {
        Credentials {
            username: username.into(),
            password: "x".into(),
            domain: None,
        }
    }

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

    #[tokio::test]
    async fn case_and_domain_do_not_buy_more_tries() {
        let v = Lockout::with_policy(AlwaysReject, 2, Duration::from_secs(60), Duration::from_secs(60));
        v.validate(&creds("Bob")).await.unwrap();
        v.validate(&creds("MAC\\BOB")).await.unwrap();
        assert!(v.is_locked("bob", Instant::now()));
    }

    #[tokio::test]
    async fn nla_attempts_count_until_the_password_is_accepted() {
        let v = Lockout::with_policy(AlwaysAccept, 3, Duration::from_secs(60), Duration::from_secs(60));
        // Two tries that NTLM turned away: the validator never saw them.
        assert!(v.attempt("alice"));
        assert!(v.attempt("alice"));
        // The third try locks the account, but it still gets its answer.
        assert!(v.attempt("alice"));
        assert!(v.is_locked("alice", Instant::now()));
        assert_eq!(v.validate(&creds("alice")).await.unwrap(), CredentialDecision::Accept);
        assert!(!v.is_locked("alice", Instant::now()));
        assert!(v.attempt("alice"), "an accepted logon clears the record");
    }

    #[tokio::test]
    async fn an_nla_logon_the_validator_rejects_counts_once() {
        let v = Lockout::with_policy(AlwaysReject, 2, Duration::from_secs(60), Duration::from_secs(60));
        assert!(v.attempt("alice"));
        assert_eq!(v.validate(&creds("alice")).await.unwrap(), CredentialDecision::Reject);
        assert!(v.attempt("alice"), "one failure so far");
        assert!(!v.attempt("alice"), "locked after the second");
    }
}
