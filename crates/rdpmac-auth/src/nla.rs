//! Network Level Authentication: the client proves who it is before a session exists.
//!
//! On a Mac outside a domain CredSSP authenticates with NTLM, which checks the client's answer
//! against the account's NT hash, the MD4 of its password in UTF-16LE, before the client hands
//! over the password itself. rdpmacd therefore keeps that hash for every account enrolled for NLA
//! (the keychain store on macOS). The password the client delegates once NTLM passed still goes
//! to the credential validator, so PAM has the last word as it has for TLS logons.

use std::io;
use std::sync::Arc;

use ironrdp_server::sspi::credssp::CredentialsProxy;
use ironrdp_server::sspi::{AuthIdentity, Username, NTLM_HASH_PREFIX};
use md4::{Digest, Md4};
use tracing::warn;

use crate::Lockout;

/// The NT hash of `password`: MD4 over its UTF-16LE encoding.
pub fn nt_hash(password: &str) -> [u8; 16] {
    let utf16: Vec<u8> = password.encode_utf16().flat_map(u16::to_le_bytes).collect();
    Md4::digest(&utf16).into()
}

/// Where NLA finds the NT hash of the account a client logs on as.
pub trait HashStore: Send + Sync {
    /// The hash enrolled for `user`, whose case does not matter; `None` if there is none.
    fn hash(&self, user: &str) -> io::Result<Option<[u8; 16]>>;
}

/// One account with a fixed password, for development (`--auth static`).
pub struct StaticHash {
    user: String,
    hash: [u8; 16],
}

impl StaticHash {
    pub fn new(user: &str, password: &str) -> Self {
        Self {
            user: user.to_owned(),
            hash: nt_hash(password),
        }
    }
}

impl HashStore for StaticHash {
    fn hash(&self, user: &str) -> io::Result<Option<[u8; 16]>> {
        Ok(user.eq_ignore_ascii_case(&self.user).then_some(self.hash))
    }
}

/// What CredSSP checks a client against: the enrolled hash of the account the client names, as
/// long as the lockout lets that account try.
pub struct NlaLookup<V> {
    store: Arc<dyn HashStore>,
    lockout: Arc<Lockout<V>>,
}

impl<V> NlaLookup<V> {
    pub fn new(store: Arc<dyn HashStore>, lockout: Arc<Lockout<V>>) -> Self {
        Self { store, lockout }
    }
}

impl<V: Send + Sync> CredentialsProxy for NlaLookup<V> {
    type AuthenticationData = AuthIdentity;

    fn auth_data_by_user(&mut self, username: &Username) -> io::Result<AuthIdentity> {
        let user = username.account_name();
        if !self.lockout.attempt(user) {
            return Err(io::Error::other("the account is locked"));
        }
        let hash = match self.store.hash(user) {
            Ok(Some(hash)) => hash,
            Ok(None) => {
                warn!(user, "NLA logon by an account that is not enrolled for it");
                return Err(io::Error::other("the account is not enrolled for NLA"));
            }
            Err(e) => {
                warn!(user, %e, "reading the account's NLA credentials failed; enrolling it again may help");
                return Err(e);
            }
        };
        let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
        Ok(AuthIdentity {
            // NTLMv2 mixes in the name and domain exactly as the client sent them.
            username: username.clone(),
            password: format!("{NTLM_HASH_PREFIX}{hex}").into(),
        })
    }

    /// Only Kerberos asks for every account up front; this lookup serves NTLM.
    fn auth_data(&mut self) -> io::Result<Vec<AuthIdentity>> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::StaticValidator;

    #[test]
    fn nt_hash_matches_the_specification() {
        // NTOWFv1("Password"), [MS-NLMP] 4.2.2.1.2.
        let expected = [
            0xa4, 0xf4, 0x9c, 0x40, 0x65, 0x10, 0xbd, 0xca, 0xb6, 0x82, 0x4e, 0xe7, 0xc3, 0x0f, 0xd8, 0x52,
        ];
        assert_eq!(nt_hash("Password"), expected);
    }

    fn lookup(max_failures: u32) -> NlaLookup<StaticValidator> {
        let lockout = Lockout::with_policy(
            StaticValidator::new("alice", "Password"),
            max_failures,
            Duration::from_secs(60),
            Duration::from_secs(60),
        );
        NlaLookup::new(Arc::new(StaticHash::new("alice", "Password")), Arc::new(lockout))
    }

    #[test]
    fn an_enrolled_account_gets_its_hash_under_the_name_the_client_sent() {
        let username = Username::new("ALICE", Some("MAC")).expect("username");
        let identity = lookup(5).auth_data_by_user(&username).expect("enrolled");
        assert_eq!(identity.username, username);
        let password: &str = identity.password.as_ref();
        assert_eq!(password, "$NTLM$:a4f49c406510bdcab6824ee7c30fd852");
    }

    #[test]
    fn other_accounts_are_refused() {
        let username = Username::new("bob", None).expect("username");
        assert!(lookup(5).auth_data_by_user(&username).is_err());
    }

    #[test]
    fn a_locked_account_is_refused() {
        let mut lookup = lookup(2);
        let username = Username::new("alice", None).expect("username");
        assert!(lookup.auth_data_by_user(&username).is_ok());
        assert!(lookup.auth_data_by_user(&username).is_ok());
        assert!(lookup.auth_data_by_user(&username).is_err(), "two tries without an accepted password");
    }
}
