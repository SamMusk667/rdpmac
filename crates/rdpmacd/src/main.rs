//! rdpmacd: RDP server for the macOS console session, built on IronRDP and libscreenio.

mod config;
mod tls;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context};
use clap::Parser;
use ironrdp_server::{CredentialValidator, RdpServer, TlsIdentityCtx};
use rdpmac_auth::{Lockout, StaticValidator};
use rdpmac_session::display::{DisplayHandler, FrameSource, ResolutionMode};
use rdpmac_session::input::InputHandler;
use rdpmac_session::monitor::{FixedMonitor, MonitorPolicy, PrimaryMonitor};
use rdpmac_session::Geometry;
use tracing::{info, warn};

use crate::config::{Args, AuthMode, Codec, Resolution, VirtualDisplay};

fn init_logging() -> anyhow::Result<()> {
    use std::io::IsTerminal;
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::builder()
        .with_default_directive(tracing::level_filters::LevelFilter::INFO.into())
        .with_env_var("RDPMAC_LOG")
        .from_env_lossy();
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().compact().with_ansi(std::io::stdout().is_terminal()))
        .with(filter)
        .try_init()
        .context("logging setup")
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
    let args = Args::parse();

    if args.request_permissions {
        let info = screenio_core::request_permissions();
        info!(?info, "permission prompts shown; grant them in System Settings and restart");
        return Ok(());
    }

    let dir = data_dir(&args)?;
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
    let display_handler = match own_display.then(rdpmac_session::virtual_screen::VirtualScreen::new).flatten() {
        Some(screen) => {
            info!("sessions get a display of their own when no screen is attached");
            display_handler.with_virtual_screen(screen)
        }
        None => display_handler,
    };
    let gfx = (args.codec == Codec::Auto).then(rdpmac_session::gfx::GfxLink::new);
    let display_handler = match &gfx {
        Some(link) => display_handler.with_gfx(link.clone()),
        None => display_handler,
    };
    info!(codec = ?args.codec, "session codec");
    let input_handler = InputHandler::spawn(geometry);
    let validator = validator(&args)?;

    let mut server = RdpServer::builder()
        .with_addr(args.listen)
        .with_tls(acceptor)
        .with_input_handler(input_handler)
        .with_display_handler(display_handler)
        .with_credential_validator(Some(validator))
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
    info!(listen = %args.listen, "rdpmacd listening");
    server.run().await.context("server stopped with an error")
}
