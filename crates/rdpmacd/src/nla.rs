//! Enrollment for NLA of the user running rdpmacd, whose login keychain holds the NT hashes.

use serde_json::{json, Value};

use crate::settings::Settings;

#[cfg(target_os = "macos")]
fn user() -> anyhow::Result<String> {
    use std::ffi::CStr;
    // SAFETY: getpwuid returns null or a record that stays valid until the next getpw* call on
    // this thread, and the name is copied out before that.
    let name = unsafe {
        let record = libc::getpwuid(libc::geteuid());
        anyhow::ensure!(!record.is_null(), "no account record for this user");
        CStr::from_ptr((*record).pw_name).to_string_lossy().into_owned()
    };
    Ok(name)
}

/// The user and when they enrolled, or null where enrollment does not apply. `stale` means the
/// hash an earlier build of rdpmacd stored cannot be read by this one, so the user has to enroll
/// again; `enrolled` is then false.
pub fn status(settings: &Settings) -> Value {
    #[cfg(target_os = "macos")]
    if settings.auth == Some(crate::config::AuthMode::Pam) {
        use rdpmac_auth::keychain::{KeychainStore, Stored};
        let Ok(user) = user() else {
            return Value::Null;
        };
        let store = KeychainStore::default();
        let state = store.stored(&user).and_then(|stored| Ok((stored, store.enrolled()?)));
        return match state {
            Ok((stored, enrolled)) => {
                let since = enrolled.iter().find(|e| e.user == user).and_then(|e| e.modified);
                let usable = matches!(stored, Stored::Hash(_));
                json!({
                    "user": user,
                    "enrolled": usable,
                    "stale": stored == Stored::Unreadable,
                    "since": since.filter(|_| usable),
                })
            }
            Err(e) => json!({ "user": user, "error": e.to_string() }),
        };
    }
    let _ = settings;
    Value::Null
}

#[cfg(target_os = "macos")]
pub async fn enroll(settings: &Settings, password: &str) -> anyhow::Result<()> {
    use ironrdp_server::{CredentialDecision, CredentialValidator, Credentials};
    use rdpmac_auth::keychain::KeychainStore;
    use rdpmac_auth::nla::nt_hash;
    use rdpmac_auth::pam::PamValidator;
    use tracing::{info, warn};

    anyhow::ensure!(
        settings.auth == Some(crate::config::AuthMode::Pam),
        "NLA enrollment is for PAM accounts; static credentials need none"
    );
    anyhow::ensure!(!password.is_empty(), "enter the password");
    let user = user()?;
    let service = settings.pam_service.clone().unwrap_or_else(|| "checkpw".into());
    let credentials = Credentials {
        username: user.clone(),
        password: password.to_owned(),
        domain: None,
    };
    match PamValidator::new(service).validate(&credentials).await {
        Ok(CredentialDecision::Accept) => {}
        Ok(CredentialDecision::Reject) => {
            warn!(user, "NLA enrollment refused: the password did not check out");
            anyhow::bail!("that is not the password of {user}");
        }
        Err(e) => anyhow::bail!("checking the password failed: {e}"),
    }
    let store = KeychainStore::default();
    if matches!(store.stored(&user), Ok(rdpmac_auth::keychain::Stored::Unreadable)) {
        info!(user, "replacing the NLA enrollment an earlier build of rdpmacd stored");
    }
    store.enroll(&user, &nt_hash(password))?;
    info!(user, "enrolled for NLA");
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn remove() -> anyhow::Result<()> {
    let user = user()?;
    if rdpmac_auth::keychain::KeychainStore::default().remove(&user)? {
        tracing::info!(user, "NLA enrollment removed");
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub async fn enroll(_: &Settings, _: &str) -> anyhow::Result<()> {
    anyhow::bail!("NLA enrollment is only implemented on macOS")
}

#[cfg(not(target_os = "macos"))]
pub fn remove() -> anyhow::Result<()> {
    anyhow::bail!("NLA enrollment is only implemented on macOS")
}
