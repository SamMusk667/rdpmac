//! The NT hashes of the accounts enrolled for NLA: one generic password per account in the login
//! keychain, under the service [`SERVICE`].
//!
//! The keychain lets the program that created an item read it back without asking and refuses
//! other programs, which it tells apart by their code signature: rdpmacd rebuilt and signed with
//! the same certificate keeps its access, other programs of the same user do not get the hashes.

use std::io;
use std::sync::Once;

use core_foundation::base::{CFType, TCFType};
use core_foundation::date::CFDate;
use core_foundation::string::CFString;
use security_framework::base::Error;
use security_framework::item::{ItemClass, ItemSearchOptions, Limit, SearchResult};
use security_framework::os::macos::keychain::SecKeychain;
use security_framework::passwords::{
    delete_generic_password, generic_password, set_generic_password_options, PasswordOptions,
};

use crate::nla::HashStore;

pub const SERVICE: &str = "com.rdpmac.nla";
/// errSecItemNotFound.
const NOT_FOUND: i32 = -25300;
/// Seconds from the Unix epoch to Core Foundation's, 2001-01-01.
const CF_EPOCH: f64 = 978_307_200.0;

#[derive(Debug, Clone, PartialEq)]
pub struct Enrollment {
    pub user: String,
    /// When the hash was last written, in seconds since the Unix epoch.
    pub modified: Option<u64>,
}

pub struct KeychainStore {
    service: String,
}

impl Default for KeychainStore {
    fn default() -> Self {
        Self::for_service(SERVICE)
    }
}

impl KeychainStore {
    fn for_service(service: &str) -> Self {
        no_dialogs();
        Self {
            service: service.to_owned(),
        }
    }

    /// Stores `hash` for `user`, replacing an earlier one.
    pub fn enroll(&self, user: &str, hash: &[u8; 16]) -> io::Result<()> {
        let mut options = PasswordOptions::new_generic_password(&self.service, user);
        options.set_label("rdpmac network level authentication");
        set_generic_password_options(hash, options).map_err(keychain_error)
    }

    /// Removes `user`'s hash; false when there was none.
    pub fn remove(&self, user: &str) -> io::Result<bool> {
        match delete_generic_password(&self.service, user) {
            Ok(()) => Ok(true),
            Err(e) if e.code() == NOT_FOUND => Ok(false),
            Err(e) => Err(keychain_error(e)),
        }
    }

    /// The enrolled accounts; listing them needs no access to the hashes.
    pub fn enrolled(&self) -> io::Result<Vec<Enrollment>> {
        let found = ItemSearchOptions::new()
            .class(ItemClass::generic_password())
            .service(&self.service)
            .load_attributes(true)
            .limit(Limit::All)
            .search();
        match found {
            Ok(results) => Ok(results.iter().filter_map(enrollment).collect()),
            Err(e) if e.code() == NOT_FOUND => Ok(Vec::new()),
            Err(e) => Err(keychain_error(e)),
        }
    }
}

impl HashStore for KeychainStore {
    fn hash(&self, user: &str) -> io::Result<Option<[u8; 16]>> {
        let enrolled = self.enrolled()?;
        let Some(account) = enrolled.iter().find(|e| e.user.eq_ignore_ascii_case(user)) else {
            return Ok(None);
        };
        let data = generic_password(PasswordOptions::new_generic_password(&self.service, &account.user))
            .map_err(keychain_error)?;
        let hash = data
            .try_into()
            .map_err(|_| io::Error::other("the keychain item does not hold an NT hash"))?;
        Ok(Some(hash))
    }
}

/// Keychain calls fail instead of showing a dialog: nobody may be at the Mac to answer it, and the
/// logon would wait for them.
fn no_dialogs() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| match SecKeychain::disable_user_interaction() {
        // The guard allows dialogs again when dropped, and rdpmacd never wants them.
        Ok(guard) => std::mem::forget(guard),
        Err(e) => tracing::warn!(%e, "could not turn keychain dialogs off"),
    });
}

fn enrollment(result: &SearchResult) -> Option<Enrollment> {
    let SearchResult::Dict(attributes) = result else {
        return None;
    };
    let attribute = |key: &'static str| {
        let key = CFString::from_static_string(key);
        // SAFETY: the dictionary's values are CF objects; wrapping one retains it.
        attributes
            .find(key.as_CFTypeRef())
            .map(|value| unsafe { CFType::wrap_under_get_rule(*value) })
    };
    // kSecAttrAccount and kSecAttrModificationDate.
    let user = attribute("acct")?.downcast::<CFString>()?.to_string();
    let modified = attribute("mdat")
        .and_then(|value| value.downcast::<CFDate>())
        .map(|date| (date.abs_time() + CF_EPOCH).max(0.0) as u64);
    Some(Enrollment { user, modified })
}

fn keychain_error(e: Error) -> io::Error {
    io::Error::other(format!("keychain: {e} ({})", e.code()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nla::nt_hash;

    #[test]
    #[ignore = "writes to the login keychain, under a service of its own that it cleans up"]
    fn enrolls_reads_lists_and_removes_a_hash() {
        let store = KeychainStore::for_service("com.rdpmac.nla.test");
        let user = "rdpmac-test-user";
        let hash = nt_hash("Password");
        store.enroll(user, &hash).expect("enroll");
        assert_eq!(store.hash("RDPMAC-Test-User").expect("read"), Some(hash));
        let enrolled = store.enrolled().expect("list");
        assert!(enrolled.iter().any(|e| e.user == user && e.modified.is_some()), "{enrolled:?}");
        assert!(store.remove(user).expect("remove"));
        assert_eq!(store.hash(user).expect("read"), None);
        assert!(!store.remove(user).expect("remove again"));
    }
}
