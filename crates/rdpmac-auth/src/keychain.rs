//! The NT hashes of the accounts enrolled for NLA: one generic password per account in the login
//! keychain, under the service [`SERVICE`].
//!
//! The keychain lets the program that wrote an item read it back without asking and refuses other
//! programs, so other programs of the same user do not get the hashes. Without an Apple team ID in
//! the signature, every build of rdpmacd counts as another program, even when signed with the same
//! certificate:
//!
//! - After an update rdpmacd cannot read the hash an earlier build wrote ([`Stored::Unreadable`]),
//!   and NLA logons fail until the account is enrolled again.
//! - Enrolling again overwrites the item, which the keychain allows, and makes it readable to the
//!   build that wrote it last.
//! - Only the build that created an item can delete it. Removing an enrollment the current build
//!   cannot delete overwrites the hash with a marker instead, which reads as no enrollment.

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
/// errSecAuthFailed: the keychain refuses this program the item.
const AUTH_FAILED: i32 = -25293;
/// errSecInvalidOwnerEdit: only the program that created the item may delete it.
const INVALID_OWNER_EDIT: i32 = -25244;
/// What a removed enrollment holds when its item could not be deleted; an NT hash is 16 bytes.
const REMOVED: &[u8] = b"removed";
/// Seconds from the Unix epoch to Core Foundation's, 2001-01-01.
const CF_EPOCH: f64 = 978_307_200.0;

#[derive(Debug, Clone, PartialEq)]
pub struct Enrollment {
    pub user: String,
    /// When the hash was last written, in seconds since the Unix epoch.
    pub modified: Option<u64>,
}

/// What the keychain holds for an account.
#[derive(Debug, Clone, PartialEq)]
pub enum Stored {
    /// No enrollment, or one removed by a build that could not delete its item.
    Nothing,
    Hash([u8; 16]),
    /// A hash another build of rdpmacd wrote, which this build may not read.
    Unreadable,
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
    /// A store under another service, for tests that must not touch real enrollments.
    pub fn for_service(service: &str) -> Self {
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
            Err(e) if e.code() == INVALID_OWNER_EDIT => {
                let mut options = PasswordOptions::new_generic_password(&self.service, user);
                options.set_label("rdpmac network level authentication");
                set_generic_password_options(REMOVED, options).map_err(keychain_error)?;
                tracing::info!(
                    user,
                    "an earlier build of rdpmacd created the NLA enrollment, which only Keychain Access can \
                     now delete; its hash is overwritten, so it admits nobody"
                );
                Ok(true)
            }
            Err(e) => Err(keychain_error(e)),
        }
    }

    /// What the keychain holds for `user`, whose case does not matter.
    pub fn stored(&self, user: &str) -> io::Result<Stored> {
        let enrolled = self.enrolled()?;
        let Some(account) = enrolled.iter().find(|e| e.user.eq_ignore_ascii_case(user)) else {
            return Ok(Stored::Nothing);
        };
        let data = generic_password(PasswordOptions::new_generic_password(&self.service, &account.user));
        classify(data.map_err(|e| (e.code(), keychain_error(e))))
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
        match self.stored(user)? {
            Stored::Hash(hash) => Ok(Some(hash)),
            Stored::Nothing => Ok(None),
            Stored::Unreadable => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "another build of rdpmacd stored the hash, which this one may not read; enroll the account again",
            )),
        }
    }
}

/// The meaning of reading an enrolled account's item: its data, or the keychain's status code
/// with the error to report.
fn classify(data: Result<Vec<u8>, (i32, io::Error)>) -> io::Result<Stored> {
    match data {
        Ok(data) if data == REMOVED => Ok(Stored::Nothing),
        Ok(data) => data
            .try_into()
            .map(Stored::Hash)
            .map_err(|_| io::Error::other("the keychain item does not hold an NT hash")),
        Err((AUTH_FAILED, _)) => Ok(Stored::Unreadable),
        Err((NOT_FOUND, _)) => Ok(Stored::Nothing),
        Err((_, e)) => Err(e),
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
    fn reads_tell_hashes_from_removed_and_unreadable_items() {
        let hash = nt_hash("Password");
        let other = || io::Error::other("keychain");
        assert_eq!(classify(Ok(hash.to_vec())).expect("hash"), Stored::Hash(hash));
        assert_eq!(classify(Ok(REMOVED.to_vec())).expect("removed"), Stored::Nothing);
        assert_eq!(classify(Err((AUTH_FAILED, other()))).expect("another build's"), Stored::Unreadable);
        assert_eq!(classify(Err((NOT_FOUND, other()))).expect("gone meanwhile"), Stored::Nothing);
        assert!(classify(Ok(vec![1, 2, 3])).is_err(), "not a hash");
        assert!(classify(Err((-25308, other()))).is_err(), "a locked keychain is an error");
    }

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
        assert_eq!(store.stored(user).expect("state"), Stored::Hash(hash));
        assert!(store.remove(user).expect("remove"));
        assert_eq!(store.hash(user).expect("read"), None);
        assert_eq!(store.stored(user).expect("state"), Stored::Nothing);
        assert!(!store.remove(user).expect("remove again"));
    }
}
