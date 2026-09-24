//! Credential validation for rdpmacd.
//!
//! IronRDP hands the credentials from the client's `ClientInfo` PDU to a [`CredentialValidator`];
//! this crate provides the validators rdpmacd wires in: [`pam::PamValidator`] for local and
//! directory accounts on macOS, [`StaticValidator`] for development, and [`Lockout`] which wraps
//! any validator with per-user failure lockout.

pub mod lockout;
#[cfg(target_os = "macos")]
pub mod pam;

use async_trait::async_trait;
pub use ironrdp_server::{CredentialDecision, CredentialValidationError, CredentialValidator, Credentials};
pub use lockout::Lockout;

/// Strips a `DOMAIN\` prefix, which some clients prepend even for local accounts.
pub fn bare_username(username: &str) -> &str {
    username.rsplit('\\').next().unwrap_or(username)
}

/// Accepts exactly one username and password and ignores the domain. Development only.
pub struct StaticValidator {
    username: String,
    password: String,
}

impl StaticValidator {
    pub fn new(username: impl Into<String>, password: impl Into<String>) -> Self {
        Self {
            username: username.into(),
            password: password.into(),
        }
    }
}

#[async_trait]
impl CredentialValidator for StaticValidator {
    async fn validate(&self, credentials: &Credentials) -> Result<CredentialDecision, CredentialValidationError> {
        let accepted = bare_username(&credentials.username) == self.username && credentials.password == self.password;
        Ok(if accepted {
            CredentialDecision::Accept
        } else {
            CredentialDecision::Reject
        })
    }
}

#[cfg(test)]
mod tests {
    use super::bare_username;

    #[test]
    fn domain_prefix_is_stripped() {
        assert_eq!(bare_username("MAC\\alice"), "alice");
        assert_eq!(bare_username("alice"), "alice");
        assert_eq!(bare_username(".\\alice"), "alice");
    }
}
