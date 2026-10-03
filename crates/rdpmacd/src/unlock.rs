//! Which logons hand their password to the lock screen (docs/unlock.md).

use std::sync::Arc;

use async_trait::async_trait;
use ironrdp_server::{CredentialDecision, CredentialValidationError, CredentialValidator, Credentials};
use rdpmac_auth::allow::Resolve;
use rdpmac_auth::bare_username;
use rdpmac_session::unlock::Unlocker;
use tracing::debug;

/// Gives the [`Unlocker`] the password of a logon that passed, when its account is the user
/// rdpmacd runs as. Another account in allow-users takes the session over with a password of
/// its own, which this user's lock screen would turn down.
pub struct Remembered {
    inner: Arc<dyn CredentialValidator>,
    unlocker: Arc<Unlocker>,
    own: u32,
    resolve: Resolve,
}

impl Remembered {
    #[cfg(target_os = "macos")]
    pub fn new(inner: Arc<dyn CredentialValidator>, unlocker: Arc<Unlocker>) -> Self {
        Self {
            inner,
            unlocker,
            // SAFETY: geteuid has no preconditions and cannot fail.
            own: unsafe { libc::geteuid() },
            resolve: rdpmac_auth::allow::uid_of,
        }
    }
}

/// Whether `user` names the account with user ID `own`, whatever the case or a domain in front.
fn is_own(user: &str, own: u32, resolve: Resolve) -> bool {
    resolve(bare_username(user)) == Some(own)
}

#[async_trait]
impl CredentialValidator for Remembered {
    async fn validate(&self, credentials: &Credentials) -> Result<CredentialDecision, CredentialValidationError> {
        let decision = self.inner.validate(credentials).await?;
        if decision == CredentialDecision::Accept {
            // A directory lookup can wait on the network when the Mac is bound to a directory.
            let (user, own, resolve) = (credentials.username.clone(), self.own, self.resolve);
            let own_account = tokio::task::spawn_blocking(move || is_own(&user, own, resolve))
                .await
                .unwrap_or(false);
            if own_account {
                self.unlocker.remember(&credentials.password);
            } else {
                debug!("another account logged on; its password is not typed into the lock screen");
            }
        }
        Ok(decision)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(name: &str) -> Option<u32> {
        match name.to_ascii_lowercase().as_str() {
            "alice" => Some(501),
            "admin" => Some(502),
            _ => None,
        }
    }

    #[test]
    fn only_the_own_account_hands_its_password_over() {
        assert!(is_own("alice", 501, resolve));
        assert!(is_own("CORP\\Alice", 501, resolve));
        assert!(!is_own("admin", 501, resolve), "an account from allow-users");
        assert!(!is_own("nobody", 501, resolve));
    }
}
