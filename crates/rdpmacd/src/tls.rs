//! Self-signed TLS identity for the daemon, generated once and reused.

use std::fs;
use std::path::Path;

use anyhow::Context;
use tracing::info;

/// Writes a self-signed ECDSA P-256 certificate and key if either file is missing.
pub fn ensure_identity(cert: &Path, key: &Path) -> anyhow::Result<()> {
    if cert.exists() && key.exists() {
        return Ok(());
    }
    let host = hostname();
    let mut params = rcgen::CertificateParams::new(vec![host.clone()]).context("certificate parameters")?;
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, host.as_str());
    let key_pair = rcgen::KeyPair::generate().context("key generation")?;
    let certificate = params.self_signed(&key_pair).context("self-signing")?;
    if let Some(dir) = cert.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    fs::write(cert, certificate.pem()).with_context(|| format!("writing {}", cert.display()))?;
    fs::write(key, key_pair.serialize_pem()).with_context(|| format!("writing {}", key.display()))?;
    restrict(key)?;
    info!(cert = %cert.display(), "generated a self-signed certificate");
    Ok(())
}

#[cfg(unix)]
fn restrict(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).context("key permissions")
}

#[cfg(not(unix))]
fn restrict(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

fn hostname() -> String {
    std::process::Command::new("hostname")
        .arg("-s")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "rdpmac".to_owned())
}
