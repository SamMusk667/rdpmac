//! Which accounts may log on to the session rdpmacd serves.
//!
//! rdpmacd serves the console session of the user it runs as, but PAM accepts any account of the
//! Mac, directory accounts too when the Mac is bound to a directory. [`Allowed`] keeps a logon by
//! any other account from reaching the password check, so the password of an account that may not
//! log on is never tried: only the user rdpmacd runs as gets through, and the accounts the
//! settings list, which then take that user's session over.

use async_trait::async_trait;
use ironrdp_server::{CredentialDecision, CredentialValidationError, CredentialValidator, Credentials};
use tracing::warn;

use crate::bare_username;

/// Looks up the user ID of the account with this name, `None` if there is none.
pub type Resolve = fn(&str) -> Option<u32>;

/// Passes a logon on to the inner validator only when its account is the user rdpmacd runs as or
/// one of the listed accounts. Accounts are compared by user ID, so neither the case of a name nor
/// a domain in front of it matters.
pub struct Allowed<V> {
    inner: V,
    own: u32,
    listed: Vec<String>,
    resolve: Resolve,
}

impl<V> Allowed<V> {
    /// `own` is the user ID of the user rdpmacd runs as; `listed` names the other accounts.
    pub fn with_resolver(inner: V, own: u32, listed: Vec<String>, resolve: Resolve) -> Self {
        Self {
            inner,
            own,
            listed,
            resolve,
        }
    }

    /// The user rdpmacd runs as and the `listed` accounts, looked up in the Mac's directory at every
    /// logon.
    #[cfg(target_os = "macos")]
    pub fn new(inner: V, listed: Vec<String>) -> Self {
        // SAFETY: geteuid has no preconditions and cannot fail.
        let own = unsafe { libc::geteuid() };
        Self::with_resolver(inner, own, listed, uid_of)
    }
}

fn may_log_on(user: &str, own: u32, listed: &[String], resolve: Resolve) -> bool {
    match resolve(user) {
        Some(uid) => uid == own || listed.iter().any(|name| resolve(name) == Some(uid)),
        None => false,
    }
}

#[async_trait]
impl<V: CredentialValidator> CredentialValidator for Allowed<V> {
    async fn validate(&self, credentials: &Credentials) -> Result<CredentialDecision, CredentialValidationError> {
        let user = bare_username(&credentials.username).to_owned();
        // A directory lookup can wait on the network when the Mac is bound to a directory.
        let (own, listed, resolve) = (self.own, self.listed.clone(), self.resolve);
        let lookup = user.clone();
        let allowed = tokio::task::spawn_blocking(move || may_log_on(&lookup, own, &listed, resolve))
            .await
            .map_err(CredentialValidationError::new)?;
        if !allowed {
            warn!(
                user,
                "logon by an account that may not take this session over; list it in allow-users to let it"
            );
            return Ok(CredentialDecision::Reject);
        }
        self.inner.validate(credentials).await
    }
}

/// The user ID of the account named `name`, local or from a directory the Mac is bound to. The
/// lookup ignores case, as logging in does.
#[cfg(target_os = "macos")]
pub fn uid_of(name: &str) -> Option<u32> {
    use std::ffi::CString;
    use std::ptr;

    let name = CString::new(name).ok()?;
    let mut buffer = vec![0 as libc::c_char; 4096];
    loop {
        // SAFETY: an all-zero passwd is a valid value to be overwritten.
        let mut record: libc::passwd = unsafe { std::mem::zeroed() };
        let mut found: *mut libc::passwd = ptr::null_mut();
        // SAFETY: every pointer is valid for the call, and the buffer length is its real length;
        // `record` only borrows from `buffer`, which outlives the use of `pw_uid` below.
        let code = unsafe { libc::getpwnam_r(name.as_ptr(), &mut record, buffer.as_mut_ptr(), buffer.len(), &mut found) };
        if code == libc::ERANGE && buffer.len() < 1 << 20 {
            buffer.resize(buffer.len() * 2, 0);
            continue;
        }
        return (code == 0 && !found.is_null()).then_some(record.pw_uid);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    static CALLS: AtomicU32 = AtomicU32::new(0);

    /// Counts the logons that reach it and accepts them all.
    struct Counting;

    #[async_trait]
    impl CredentialValidator for Counting {
        async fn validate(&self, _: &Credentials) -> Result<CredentialDecision, CredentialValidationError> {
            CALLS.fetch_add(1, Ordering::SeqCst);
            Ok(CredentialDecision::Accept)
        }
    }

    fn directory(name: &str) -> Option<u32> {
        match name.to_lowercase().as_str() {
            "alice" => Some(501),
            "bob" => Some(502),
            "carol" => Some(503),
            _ => None,
        }
    }

    fn creds(username: &str) -> Credentials {
        Credentials {
            username: username.into(),
            password: "x".into(),
            domain: None,
        }
    }

    async fn decide(listed: &[&str], username: &str) -> (CredentialDecision, u32) {
        let allowed = Allowed::with_resolver(Counting, 501, listed.iter().map(|s| s.to_string()).collect(), directory);
        let before = CALLS.load(Ordering::SeqCst);
        let decision = allowed.validate(&creds(username)).await.expect("decided");
        (decision, CALLS.load(Ordering::SeqCst) - before)
    }

    // One test, so that the call counter is not shared between tests running at once.
    #[tokio::test]
    async fn only_the_own_user_and_listed_accounts_reach_the_password_check() {
        assert_eq!(decide(&[], "alice").await, (CredentialDecision::Accept, 1));
        assert_eq!(decide(&[], "MAC\\ALICE").await, (CredentialDecision::Accept, 1), "case and domain");
        assert_eq!(decide(&[], "bob").await, (CredentialDecision::Reject, 0), "another account");
        assert_eq!(decide(&[], "mallory").await, (CredentialDecision::Reject, 0), "no such account");
        assert_eq!(decide(&[], "").await, (CredentialDecision::Reject, 0));
        assert_eq!(decide(&["Bob"], "bob").await, (CredentialDecision::Accept, 1), "listed");
        assert_eq!(decide(&["bob"], "carol").await, (CredentialDecision::Reject, 0), "not listed");
        assert_eq!(decide(&["mallory"], "mallory").await, (CredentialDecision::Reject, 0), "listed but unknown");
    }

    /// Reads the real directory, never changes it.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_directory_knows_this_user_in_any_case() {
        // SAFETY: geteuid has no preconditions and cannot fail.
        let own = unsafe { libc::geteuid() };
        let name = std::env::var("USER").expect("USER is set");
        assert_eq!(uid_of(&name), Some(own));
        assert_eq!(uid_of(&name.to_uppercase()), Some(own));
        assert_eq!(uid_of("rdpmac-no-such-user-8f3a"), None);
        assert_eq!(uid_of("nul\0in name"), None);
    }
}
