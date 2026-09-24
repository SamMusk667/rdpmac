//! rdpmacd: RDP server for the macOS console session, built on IronRDP and libscreenio.

mod config;
mod tls;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context};
use clap::Parser;
use ironrdp_server::{CredentialValidator, RdpServer, TlsIdentityCtx};
use rdpmac_auth::{Lockout, StaticValidator};
use rdpmac_session::display::{DisplayHandler, FrameSource};
use rdpmac_session::input::InputHandler;
use rdpmac_session::monitor::{FixedMonitor, MonitorPolicy, PrimaryMonitor};
use rdpmac_session::Geometry;
use tracing::{info, warn};

use crate::config::{Args, AuthMode};

fn init_logging() -> anyhow::Result<()> {
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::builder()
        .with_default_directive(tracing::level_filters::LevelFilter::INFO.into())
        .with_env_var("RDPMAC_LOG")
        .from_env_lossy();
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().compact())
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
        (_, Some((width, height))) => Geometry {
            id: 0,
            x: 0,
            y: 0,
            width,
            height,
            scale: 1.0,
        },
        (Some(chosen), None) => {
            info!(
                display = chosen.id,
                width = chosen.width,
                height = chosen.height,
                scale = chosen.scale,
                session = ?screenio_core::session_info(),
                "serving display"
            );
            Geometry::from_display(&chosen)
        }
        (None, None) => {
            // Displays asleep or detached at startup: keep serving, the capture loop retries.
            warn!(available = ?displays.iter().map(|d| d.id).collect::<Vec<_>>(), "no matching display yet");
            Geometry {
                id: args.display.unwrap_or(0),
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
                scale: 1.0,
            }
        }
    };
    let geometry = rdpmac_session::shared(initial);
    if args.test_pattern.is_none() && !screenio_core::session_info().can_capture {
        warn!("screen recording permission is missing; connections will see no picture");
    }

    let source = match args.test_pattern {
        Some((width, height)) => {
            warn!(width, height, "serving a test pattern instead of the screen");
            FrameSource::TestPattern { width, height }
        }
        None => FrameSource::Screen,
    };
    let display_handler = DisplayHandler::with_source(policy, geometry.clone(), source, args.fps, args.cursor_hz);
    let input_handler = InputHandler::spawn(geometry);
    let validator = validator(&args)?;

    let mut server = RdpServer::builder()
        .with_addr(args.listen)
        .with_tls(acceptor)
        .with_input_handler(input_handler)
        .with_display_handler(display_handler)
        .with_credential_validator(Some(validator))
        .build();
    info!(listen = %args.listen, "rdpmacd listening");
    server.run().await.context("server stopped with an error")
}
