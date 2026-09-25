//! rdpmacd: RDP server for the macOS console session, built on IronRDP and libscreenio.

mod config;
mod control;
mod display_mode;
mod nla;
mod settings;
mod status;
mod tls;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context};
use clap::{CommandFactory, FromArgMatches};
use ironrdp_server::sspi::credssp::CredentialsProxy;
use ironrdp_server::sspi::AuthIdentity;
use ironrdp_server::{CredentialValidator, RdpServer, TlsIdentityCtx};
use rdpmac_auth::nla::{NlaLookup, StaticHash};
use rdpmac_auth::{Lockout, StaticValidator};
use rdpmac_session::display::{DisplayHandler, FrameSource, ResolutionMode};
use rdpmac_session::input::InputHandler;
use rdpmac_session::monitor::{FixedMonitor, MonitorPolicy, PrimaryMonitor};
use rdpmac_session::Geometry;
use tracing::{info, warn};

use crate::config::{Args, AuthMode, Codec, Resolution, Security, VirtualDisplay};
use crate::settings::Settings;

/// Where the daemon keeps its own log files: `RDPMAC_LOG_DIR`, with a leading `~/` meaning the
/// home directory, since a launchd job inside the app bundle cannot spell out the home path.
fn log_dir() -> Option<PathBuf> {
    let dir = std::env::var("RDPMAC_LOG_DIR").ok().filter(|d| !d.is_empty())?;
    match dir.strip_prefix("~/") {
        Some(rest) => directories::BaseDirs::new().map(|base| base.home_dir().join(rest)),
        None => Some(PathBuf::from(dir)),
    }
}

fn init_logging() -> anyhow::Result<()> {
    use std::io::IsTerminal;
    use tracing_appender::rolling::{Builder, Rotation};
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::builder()
        .with_default_directive(tracing::level_filters::LevelFilter::INFO.into())
        .with_env_var("RDPMAC_LOG")
        .from_env_lossy();
    let registry = tracing_subscriber::registry().with(filter);
    match log_dir() {
        // Under launchd nothing reads stdout; keep two weeks of daily files instead.
        Some(dir) => {
            // The appender prunes old files before it creates the directory.
            std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
            let files = Builder::new()
                .rotation(Rotation::DAILY)
                .filename_prefix("rdpmacd")
                .filename_suffix("log")
                .max_log_files(14)
                .build(&dir)
                .with_context(|| format!("log files in {}", dir.display()))?;
            registry
                .with(tracing_subscriber::fmt::layer().compact().with_ansi(false).with_writer(files))
                .try_init()
        }
        None => registry
            .with(tracing_subscriber::fmt::layer().compact().with_ansi(std::io::stdout().is_terminal()))
            .try_init(),
    }
    .context("logging setup")?;
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic| {
        tracing::error!(%panic, "rdpmacd panicked");
        default_hook(panic);
    }));
    Ok(())
}

fn data_dir(args: &Args) -> anyhow::Result<PathBuf> {
    if let Some(dir) = &args.data_dir {
        return Ok(dir.clone());
    }
    directories::ProjectDirs::from("", "", "rdpmac")
        .map(|d| d.data_dir().to_path_buf())
        .context("no home directory to place rdpmac data in")
}

fn validator(args: &Args) -> anyhow::Result<Arc<dyn CredentialValidator>> {
    Ok(match args.auth {
        AuthMode::Static => {
            let (user, password) = match (&args.user, &args.password) {
                (Some(u), Some(p)) => (u.clone(), p.clone()),
                _ => bail!("--auth static needs --user and --password"),
            };
            warn!("static credentials in use; this mode is for development only");
            Arc::new(Lockout::new(StaticValidator::new(user, password)))
        }
        AuthMode::Pam => {
            #[cfg(target_os = "macos")]
            {
                Arc::new(Lockout::new(rdpmac_auth::pam::PamValidator::new(args.pam_service.clone())))
            }
            #[cfg(not(target_os = "macos"))]
            {
                bail!("PAM authentication is only implemented on macOS; use --auth static")
            }
        }
    })
}

type Lookup = Box<dyn CredentialsProxy<AuthenticationData = AuthIdentity> + Send>;

/// For NLA: the validator for the password a client delegates, and the lookup CredSSP checks the
/// client against before that. Both count failed logons against the same lockout.
fn nla(args: &Args) -> anyhow::Result<(Arc<dyn CredentialValidator>, Lookup)> {
    Ok(match args.auth {
        AuthMode::Static => {
            let (user, password) = match (&args.user, &args.password) {
                (Some(u), Some(p)) => (u.clone(), p.clone()),
                _ => bail!("--auth static needs --user and --password"),
            };
            warn!("static credentials in use; this mode is for development only");
            let store = Arc::new(StaticHash::new(&user, &password));
            let lockout = Arc::new(Lockout::new(StaticValidator::new(user, password)));
            (lockout.clone(), Box::new(NlaLookup::new(store, lockout)))
        }
        AuthMode::Pam => {
            #[cfg(target_os = "macos")]
            {
                use rdpmac_auth::keychain::KeychainStore;
                let store = KeychainStore::default();
                match store.enrolled() {
                    Ok(enrolled) if enrolled.is_empty() => warn!(
                        "NLA is on but no account is enrolled for it, so nobody can log on; enroll in the \
                         rdpmac app"
                    ),
                    Ok(enrolled) => {
                        info!(accounts = ?enrolled.iter().map(|e| &e.user).collect::<Vec<_>>(), "enrolled for NLA")
                    }
                    Err(e) => warn!(%e, "listing the accounts enrolled for NLA failed"),
                }
                let lockout = Arc::new(Lockout::new(rdpmac_auth::pam::PamValidator::new(args.pam_service.clone())));
                (lockout.clone(), Box::new(NlaLookup::new(Arc::new(store), lockout)))
            }
            #[cfg(not(target_os = "macos"))]
            {
                bail!("PAM authentication is only implemented on macOS; use --auth static")
            }
        }
    })
}

/// macOS checks both permissions against the process responsible for rdpmacd: rdpmacd itself when
/// launchd starts it, otherwise the terminal app or, over SSH, sshd.
fn warn_missing_permissions(info: &screenio_core::SessionInfo) {
    if !info.can_capture {
        warn!("screen recording permission is missing; connections will see no picture");
    }
    if !info.can_inject {
        warn!("accessibility permission is missing; keyboard and mouse input from clients is dropped");
    }
    if info.can_capture && info.can_inject {
        return;
    }
    let over_ssh = std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_CLIENT").is_some();
    // System Settings lists Terminal under its app name, not the TERM_PROGRAM value.
    match std::env::var("TERM_PROGRAM").map(|app| if app == "Apple_Terminal" { "Terminal".into() } else { app }) {
        _ if over_ssh => warn!(
            "started over SSH, so macOS checks the permissions of sshd, not rdpmacd; \
             run rdpmacd as a LaunchAgent: sh scripts/agent.sh install"
        ),
        Ok(app) => warn!(
            "started from {app}, so macOS checks the permissions of {app}, not rdpmacd; \
             grant them to {app} or run rdpmacd as a LaunchAgent: sh scripts/agent.sh install"
        ),
        Err(_) => warn!(
            "turn rdpmacd on under System Settings > Privacy & Security for both permissions, \
             then restart it"
        ),
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    init_logging()?;
    let matches = Args::command().get_matches();
    let mut args = Args::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());

    if let Some((display, width, height)) = args.switch_display_mode {
        return screenio_core::switch_display_mode(display, width, height)
            .with_context(|| format!("switching display {display} to {width}x{height}"));
    }

    if args.request_permissions {
        let info = screenio_core::request_permissions();
        info!(?info, "permission prompts shown; grant them in System Settings and restart");
        return Ok(());
    }

    let dir = data_dir(&args)?;
    let config_path = args.config.clone().unwrap_or_else(|| dir.join("config.toml"));
    Settings::load(&config_path)?.apply(&mut args, &matches);
    let cert = args.cert.clone().unwrap_or_else(|| dir.join("cert.pem"));
    let key = args.key.clone().unwrap_or_else(|| dir.join("key.pem"));
    tls::ensure_identity(&cert, &key)?;
    let identity = TlsIdentityCtx::init_from_paths(&cert, &key).context("loading the TLS identity")?;
    let acceptor = identity.make_acceptor().context("building the TLS acceptor")?;

    let policy: Arc<dyn MonitorPolicy> = match args.display {
        Some(id) => Arc::new(FixedMonitor(id)),
        None => Arc::new(PrimaryMonitor),
    };
    let displays = screenio_core::list_displays().unwrap_or_else(|e| {
        warn!(%e, "listing displays failed");
        Vec::new()
    });
    let initial = match (policy.select(&displays), args.test_pattern) {
        (_, Some((width, height))) => Geometry::synthetic(width, height),
        (Some(chosen), None) => {
            info!(
                display = chosen.id,
                width = chosen.width,
                height = chosen.height,
                scale = chosen.scale,
                session = ?screenio_core::session_info(),
                "serving display"
            );
            Geometry::native(&chosen)
        }
        (None, None) => {
            // Displays asleep or detached at startup: keep serving, the capture loop retries.
            warn!(available = ?displays.iter().map(|d| d.id).collect::<Vec<_>>(), "no matching display yet");
            Geometry::synthetic(1920, 1080)
        }
    };
    let geometry = rdpmac_session::shared(initial);
    if args.test_pattern.is_none() {
        warn_missing_permissions(&screenio_core::session_info());
    }

    let source = match args.test_pattern {
        Some((width, height)) => {
            warn!(width, height, "serving a test pattern instead of the screen");
            FrameSource::TestPattern { width, height }
        }
        None => FrameSource::Screen,
    };
    let mode = match args.resolution {
        Resolution::FollowClient => ResolutionMode::FollowClient,
        Resolution::Native => ResolutionMode::Native,
    };
    info!(?mode, "session resolution");
    let display_handler = DisplayHandler::new(policy, geometry.clone(), source, mode, args.fps, args.cursor_hz);
    // A display of its own only replaces the primary display; a chosen display is served as is.
    let own_display = args.virtual_display == VirtualDisplay::Auto
        && mode == ResolutionMode::FollowClient
        && args.test_pattern.is_none()
        && args.display.is_none();
    let virtual_screen = own_display
        .then(|| rdpmac_session::virtual_screen::VirtualScreen::new(display_mode::switch_in_helper))
        .flatten();
    let display_handler = match &virtual_screen {
        Some(screen) => {
            info!("sessions get a display of their own when no screen is attached");
            display_handler.with_virtual_screen(screen.clone())
        }
        None => display_handler,
    };
    let gfx = (args.codec == Codec::Auto).then(rdpmac_session::gfx::GfxLink::new);
    let display_handler = match &gfx {
        Some(link) => display_handler.with_gfx(link.clone()),
        None => display_handler,
    };
    info!(codec = ?args.codec, "session codec");
    let status_geometry = geometry.clone();
    let input_handler = InputHandler::spawn(geometry);
    let tracker = Arc::new(status::Tracker::default());
    let (validator, lookup) = match args.security {
        Security::Tls => (validator(&args)?, None),
        Security::Nla => {
            let (validator, lookup) = nla(&args)?;
            (validator, Some(lookup))
        }
    };
    info!(security = ?args.security, "client authentication");
    let validator: Arc<dyn CredentialValidator> = Arc::new(status::Recorded::new(validator, tracker.clone()));

    let server = RdpServer::builder().with_addr(args.listen);
    let server = match args.security {
        Security::Tls => server.with_tls(acceptor),
        Security::Nla => server.with_hybrid(acceptor, identity.pub_key.clone()),
    };
    let mut server = server
        .with_input_handler(input_handler)
        .with_display_handler(display_handler)
        .with_credential_validator(Some(validator))
        .with_connection_handler(Some(Box::new(status::Connections(tracker.clone()))))
        // Adopt the size the client asks for in its connection request instead of the display's;
        // DisplayHandler::request_initial_size then serves exactly that size.
        .with_honor_client_desktop_size(mode == ResolutionMode::FollowClient)
        .with_cliprdr_factory((!args.no_clipboard).then(|| {
            Box::new(rdpmac_session::clipboard::ClipboardFactory::new())
                as Box<dyn ironrdp_server::CliprdrServerFactory>
        }))
        .with_gfx_factory(gfx.map(|link| {
            Box::new(rdpmac_session::gfx::GfxFactory::new(link)) as Box<dyn ironrdp_server::GfxServerFactory>
        }))
        .build();
    server.set_credentials_lookup(lookup);
    let log_dir = log_dir()
        .or_else(|| directories::BaseDirs::new().map(|base| base.home_dir().join("Library/Logs/rdpmac")))
        .unwrap_or_else(|| dir.clone());
    let control = Arc::new(control::Control {
        tracker,
        geometry: status_geometry,
        virtual_screen,
        effective: Settings::effective(&args),
        config_path,
        data_dir: dir,
        log_dir,
        cert,
        key,
        started: status::now(),
        permissions_at_start: screenio_core::session_info(),
    });
    tokio::spawn(async move {
        if let Err(e) = control::serve(control).await {
            warn!("control socket unavailable: {e:#}");
        }
    });
    info!(listen = %args.listen, "rdpmacd listening");
    server.run().await.context("server stopped with an error")
}
