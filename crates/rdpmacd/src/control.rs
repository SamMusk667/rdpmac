//! The control socket for the menu-bar app: one JSON request per line on a Unix socket in the
//! data directory, answered by one JSON line. Only processes of the same user may connect.
//!
//! Requests are `{"cmd": ...}` with `status`, `request_permissions`, `get_config`,
//! `set_config` (`settings`: the whole file), `import_certificate` (`cert_pem`, `key_pem`),
//! `nla_enroll` (`password`), `nla_remove` and `restart`. Every answer carries `"ok"`, and
//! `"error"` when it is false. `set_config` answers `restart_required`, which is false when the
//! changes reach the next connection without a restart. The status says `restart_needed` when a permission was granted
//! after the daemon started, and under `nla` when the user running rdpmacd enrolled for NLA.

use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use clap::{ArgMatches, FromArgMatches};
use ironrdp_server::tokio_rustls::rustls::pki_types::{pem::PemObject, CertificateDer};
use ironrdp_server::TlsIdentityCtx;
use rdpmac_session::gfx::GfxLink;
use rdpmac_session::virtual_screen::VirtualScreen;
use rdpmac_session::SharedGeometry;
use serde::Deserialize;
use serde_json::{json, Value};
use sha1::Digest;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tracing::{debug, info, warn};

use crate::config::Args;
use crate::nla;
use crate::settings::Settings;
use crate::status::Tracker;

/// launchd restarts the agent after an exit with a non-zero status.
pub const EXIT_RESTART: i32 = 75;
/// A request line may carry a certificate chain and its key.
const MAX_REQUEST: u64 = 1 << 20;

#[derive(Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case", deny_unknown_fields)]
enum Command {
    Status,
    RequestPermissions,
    GetConfig,
    SetConfig { settings: Settings },
    ImportCertificate { cert_pem: String, key_pem: String },
    /// Checks the password of the user running rdpmacd and enrolls that user for NLA.
    NlaEnroll { password: String },
    NlaRemove,
    Restart,
}

pub struct Control {
    pub tracker: Arc<Tracker>,
    pub geometry: SharedGeometry,
    pub virtual_screen: Option<Arc<VirtualScreen>>,
    /// Flags and file combined, as the daemon runs.
    pub effective: Settings,
    pub config_path: PathBuf,
    pub data_dir: PathBuf,
    pub log_dir: PathBuf,
    pub cert: PathBuf,
    pub key: PathBuf,
    pub started: u64,
    /// The permissions when the daemon started. macOS applies a permission granted later only
    /// to a new process, and the screen recording check keeps answering as it did at launch.
    pub permissions_at_start: screenio_core::SessionInfo,
    /// The graphics pipeline, when H.264 is on, whose codec and conversion choices saved settings
    /// change for the next connection.
    pub gfx: Option<Arc<GfxLink>>,
    /// The command line, whose values win over saved settings.
    pub matches: ArgMatches,
}

pub fn socket_path(data_dir: &Path) -> PathBuf {
    data_dir.join("control.sock")
}

pub async fn serve(control: Arc<Control>) -> anyhow::Result<()> {
    let path = socket_path(&control.data_dir);
    // A socket left by a daemon that did not exit cleanly blocks bind.
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("removing {}", path.display())),
    }
    let listener = UnixListener::bind(&path).with_context(|| format!("binding {}", path.display()))?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).context("control socket permissions")?;
    info!(path = %path.display(), "control socket ready");
    loop {
        let (stream, _) = listener.accept().await.context("accepting a control connection")?;
        if !same_user(&stream) {
            warn!("refused a control connection from another user");
            continue;
        }
        let control = control.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(&control, stream).await {
                debug!(%e, "control connection ended");
            }
        });
    }
}

fn same_user(stream: &UnixStream) -> bool {
    // SAFETY: geteuid has no preconditions and cannot fail.
    let uid = unsafe { libc::geteuid() };
    stream.peer_cred().is_ok_and(|cred| cred.uid() == uid)
}

async fn handle(control: &Control, stream: UnixStream) -> anyhow::Result<()> {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut line = String::new();
    loop {
        line.clear();
        let read = (&mut reader).take(MAX_REQUEST + 1).read_line(&mut line).await?;
        if read == 0 {
            return Ok(());
        }
        let too_long = read as u64 > MAX_REQUEST;
        let (answer, restart) = match serde_json::from_str::<Command>(&line) {
            _ if too_long => (failure("request too large"), false),
            Ok(Command::Restart) => (json!({ "ok": true }), true),
            Ok(Command::NlaEnroll { password }) => (control.enroll(password).await, false),
            Ok(command) => (control.answer(command), false),
            Err(e) => (failure(format!("bad request: {e}")), false),
        };
        let mut encoded = serde_json::to_vec(&answer)?;
        encoded.push(b'\n');
        write.write_all(&encoded).await?;
        write.flush().await?;
        if restart {
            info!("restarting at the control socket's request");
            std::process::exit(EXIT_RESTART);
        }
        if too_long {
            return Ok(());
        }
    }
}

fn failure(error: impl std::fmt::Display) -> Value {
    json!({ "ok": false, "error": error.to_string() })
}

impl Control {
    fn answer(&self, command: Command) -> Value {
        match command {
            Command::Status => self.status(),
            Command::RequestPermissions => {
                let info = screenio_core::request_permissions();
                json!({ "ok": true, "permissions": permissions(&info) })
            }
            Command::GetConfig => match Settings::load(&self.config_path) {
                Ok(settings) => json!({ "ok": true, "settings": settings, "path": self.config_path }),
                Err(e) => failure(format!("{e:#}")),
            },
            Command::SetConfig { settings } => match settings.save(&self.config_path) {
                Ok(()) => {
                    info!(path = %self.config_path.display(), "settings saved");
                    json!({ "ok": true, "restart_required": self.apply(&settings) })
                }
                Err(e) => failure(format!("{e:#}")),
            },
            Command::ImportCertificate { cert_pem, key_pem } => {
                match install_certificate(&self.cert, &self.key, &cert_pem, &key_pem) {
                    Ok(()) => {
                        info!(cert = %self.cert.display(), "certificate imported");
                        json!({ "ok": true, "restart_required": true, "certificate": certificate(&self.cert) })
                    }
                    Err(e) => failure(format!("{e:#}")),
                }
            }
            Command::NlaRemove => match nla::remove() {
                Ok(()) => json!({ "ok": true, "nla": nla::status(&self.effective) }),
                Err(e) => failure(format!("{e:#}")),
            },
            // Answered by `handle`, which has to exit after replying.
            Command::Restart => json!({ "ok": true }),
            // Answered by `handle`, since checking the password takes a while.
            Command::NlaEnroll { .. } => failure("enrollment is answered asynchronously"),
        }
    }

    /// Hands saved settings that need no restart to the next connection; says whether the rest
    /// needs one.
    fn apply(&self, saved: &Settings) -> bool {
        let mut args = match Args::from_arg_matches(&self.matches) {
            Ok(args) => args,
            Err(e) => {
                warn!(%e, "reading the command line again failed");
                return true;
            }
        };
        saved.apply(&mut args, &self.matches);
        if let Some(gfx) = &self.gfx {
            let options = crate::gfx_options(&args);
            info!(?options, "the next connection uses the saved codec settings");
            gfx.set_options(options);
        }
        self.effective.restart_needed(&Settings::effective(&args))
    }

    async fn enroll(&self, password: String) -> Value {
        match nla::enroll(&self.effective, &password).await {
            Ok(()) => json!({ "ok": true, "nla": nla::status(&self.effective) }),
            Err(e) => failure(format!("{e:#}")),
        }
    }

    fn status(&self) -> Value {
        let info = screenio_core::session_info();
        let connection = self.tracker.current();
        let size = *self.geometry.lock().unwrap_or_else(|e| e.into_inner());
        let ours = self.virtual_screen.as_ref().and_then(|screen| screen.current_id());
        let displays: Vec<Value> = screenio_core::list_displays()
            .unwrap_or_default()
            .iter()
            .map(|d| {
                json!({
                    "id": d.id,
                    "width": d.width,
                    "height": d.height,
                    "scale": d.scale,
                    "primary": d.primary,
                    "placeholder": d.placeholder,
                    "virtual": Some(d.id) == ours,
                })
            })
            .collect();
        json!({
            "ok": true,
            "version": env!("RDPMAC_VERSION"),
            "pid": std::process::id(),
            "started": self.started,
            "permissions": permissions(&info),
            "restart_needed": granted_since_start(&self.permissions_at_start, &info),
            "nla": nla::status(&self.effective),
            "connection": connection,
            "session_size": connection.as_ref().map(|_| [size.width, size.height]),
            "last_connection": self.tracker.last(),
            "displays": displays,
            "virtual_displays_supported": self.virtual_screen.is_some(),
            "settings": self.effective,
            "certificate": certificate(&self.cert),
            "config_path": self.config_path,
            "data_dir": self.data_dir,
            "log_dir": self.log_dir,
        })
    }
}

fn permissions(info: &screenio_core::SessionInfo) -> Value {
    json!({ "screen_recording": info.can_capture, "accessibility": info.can_inject })
}

fn granted_since_start(at_start: &screenio_core::SessionInfo, now: &screenio_core::SessionInfo) -> bool {
    (now.can_capture && !at_start.can_capture) || (now.can_inject && !at_start.can_inject)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(":")
}

/// Path and thumbprints of the server certificate, which mstsc shows when it asks whether to
/// trust the server.
fn certificate(path: &Path) -> Value {
    let first = CertificateDer::pem_file_iter(path)
        .ok()
        .and_then(|mut certs| certs.next())
        .and_then(Result::ok);
    match first {
        Some(der) => json!({
            "path": path,
            "sha1": hex(&sha1::Sha1::digest(der.as_ref())),
            "sha256": hex(&sha2::Sha256::digest(der.as_ref())),
        }),
        None => json!({ "path": path }),
    }
}

/// Stages the new pair next to the old one, checks it the way the daemon loads it at startup,
/// keeps the old pair as `*.previous.pem` and moves the new one in place.
fn install_certificate(cert: &Path, key: &Path, cert_pem: &str, key_pem: &str) -> anyhow::Result<()> {
    // IronRDP parses a file as PEM only when its name ends in .pem.
    let staged_cert = cert.with_extension("new.pem");
    let staged_key = key.with_extension("new.pem");
    let write = |path: &Path, text: &str| -> anyhow::Result<()> {
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("writing {}", path.display()))?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        Ok(())
    };
    let staged = write(&staged_cert, cert_pem)
        .and_then(|()| write(&staged_key, key_pem))
        .and_then(|()| {
            TlsIdentityCtx::init_from_paths(&staged_cert, &staged_key)?
                .make_acceptor()
                .context("the certificate and key do not form a usable TLS identity")?;
            Ok(())
        });
    if let Err(e) = staged {
        let _ = fs::remove_file(&staged_cert);
        let _ = fs::remove_file(&staged_key);
        return Err(e);
    }
    for (current, staged) in [(cert, &staged_cert), (key, &staged_key)] {
        match fs::rename(current, current.with_extension("previous.pem")) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("keeping {}", current.display())),
        }
        fs::rename(staged, current).with_context(|| format!("installing {}", current.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rdpmacd-control-{name}-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn identity() -> (String, String) {
        let key = rcgen::KeyPair::generate().expect("key");
        let params = rcgen::CertificateParams::new(vec!["test.example".into()]).expect("params");
        let cert = params.self_signed(&key).expect("cert");
        (cert.pem(), key.serialize_pem())
    }

    #[test]
    fn requests_parse_from_json_lines() {
        assert!(matches!(serde_json::from_str::<Command>(r#"{"cmd":"status"}"#), Ok(Command::Status)));
        let set = serde_json::from_str::<Command>(r#"{"cmd":"set_config","settings":{"fps":20}}"#);
        assert!(matches!(set, Ok(Command::SetConfig { settings }) if settings.fps == Some(20)));
        assert!(serde_json::from_str::<Command>(r#"{"cmd":"format_disk"}"#).is_err());
    }

    #[test]
    fn import_keeps_the_old_pair_and_rejects_garbage() {
        let dir = scratch("import");
        let (cert, key) = (dir.join("cert.pem"), dir.join("key.pem"));
        let (old_cert, old_key) = identity();
        fs::write(&cert, &old_cert).expect("old cert");
        fs::write(&key, &old_key).expect("old key");

        assert!(install_certificate(&cert, &key, "not a certificate", "nor a key").is_err());
        assert_eq!(fs::read_to_string(&cert).expect("cert"), old_cert, "a bad import changes nothing");
        assert!(!dir.join("cert.new.pem").exists());

        let (new_cert, new_key) = identity();
        install_certificate(&cert, &key, &new_cert, &new_key).expect("imported");
        assert_eq!(fs::read_to_string(&cert).expect("cert"), new_cert);
        assert_eq!(fs::read_to_string(dir.join("cert.previous.pem")).expect("backup"), old_cert);
        let mode = fs::metadata(&key).expect("key").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert!(certificate(&cert)["sha256"].as_str().is_some_and(|s| s.len() == 95));
        fs::remove_dir_all(&dir).expect("cleanup");
    }
}
