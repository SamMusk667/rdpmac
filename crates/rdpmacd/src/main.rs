//! rdpmacd: RDP server for the macOS console session, built on IronRDP and libscreenio.

mod config;
mod tls;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context};
use clap::Parser;
use ironrdp_server::{CredentialValidator, RdpServer, TlsIdentityCtx};
use rdpmac_auth::{Lockout, StaticValidator};
use rdpmac_session::display::DisplayHandler;
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
    let displays = screenio_core::list_displays().context("listing displays")?;
    let Some(chosen) = policy.select(&displays) else {
        bail!("no matching display; available: {:?}", displays.iter().map(|d| d.id).collect::<Vec<_>>());
    };
    let geometry = rdpmac_session::shared(Geometry::from_display(&chosen));
    info!(
        display = chosen.id,
        width = chosen.width,
        height = chosen.height,
        scale = chosen.scale,
        session = ?screenio_core::session_info(),
        "serving display"
    );
    if !screenio_core::session_info().can_capture {
        warn!("screen recording permission is missing; connections will see no picture");
    }

    let display_handler = DisplayHandler::new(policy, geometry.clone(), args.fps, args.cursor_hz);
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
