use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Parser, ValueEnum};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum AuthMode {
    /// Local and directory accounts through PAM.
    Pam,
    /// One fixed username and password, for development.
    Static,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Resolution {
    /// The size the client asks for (mstsc /w /h, full screen, window resizing); the display is
    /// scaled to it when the two differ.
    FollowClient,
    /// The display's own pixel size.
    Native,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Codec {
    /// H.264 through the graphics pipeline when the client supports it, RemoteFX otherwise.
    Auto,
    /// RemoteFX and bitmap updates only.
    Remotefx,
}

#[derive(Debug, Parser)]
#[command(name = "rdpmacd", version, about = "RDP server for the macOS console session")]
pub struct Args {
    /// Address to listen on.
    #[arg(long, default_value = "0.0.0.0:3389")]
    pub listen: SocketAddr,

    /// CoreGraphics display id to serve; defaults to the primary display.
    #[arg(long)]
    pub display: Option<u32>,

    /// Upper bound on captured frames per second.
    #[arg(long, default_value_t = 30)]
    pub fps: u32,

    /// Cursor polling rate.
    #[arg(long, default_value_t = 30)]
    pub cursor_hz: u32,

    /// Video codec for the session picture.
    #[arg(long, value_enum, default_value_t = Codec::Auto)]
    pub codec: Codec,

    /// Do not share the clipboard with clients.
    #[arg(long)]
    pub no_clipboard: bool,

    /// How the session resolution is chosen.
    #[arg(long, value_enum, default_value_t = Resolution::FollowClient)]
    pub resolution: Resolution,

    #[arg(long, value_enum, default_value_t = AuthMode::Pam)]
    pub auth: AuthMode,

    /// PAM service to authenticate against.
    #[arg(long, default_value = "checkpw")]
    pub pam_service: String,

    /// Username accepted in static mode.
    #[arg(long, required_if_eq("auth", "static"))]
    pub user: Option<String>,

    /// Password accepted in static mode.
    #[arg(long, required_if_eq("auth", "static"))]
    pub password: Option<String>,

    /// PEM certificate; generated under the data directory when absent.
    #[arg(long)]
    pub cert: Option<PathBuf>,

    /// PEM private key matching `--cert`.
    #[arg(long)]
    pub key: Option<PathBuf>,

    /// Directory for generated state; defaults to ~/Library/Application Support/rdpmac.
    #[arg(long)]
    pub data_dir: Option<PathBuf>,

    /// Serve a synthetic moving picture of this size (for example 1920x1080) instead of the
    /// screen. Needs no permission; used to test clients and measure encoding cost.
    #[arg(long, value_parser = parse_size)]
    pub test_pattern: Option<(u32, u32)>,

    /// Trigger the macOS permission prompts for screen recording and accessibility, then exit.
    #[arg(long)]
    pub request_permissions: bool,
}

fn parse_size(text: &str) -> Result<(u32, u32), String> {
    let (w, h) = text
        .split_once(['x', 'X'])
        .ok_or_else(|| format!("expected WIDTHxHEIGHT, got {text}"))?;
    let parse = |v: &str| v.trim().parse::<u32>().map_err(|e| format!("{v}: {e}"));
    let (w, h) = (parse(w)?, parse(h)?);
    if !(16..=8192).contains(&w) || !(16..=8192).contains(&h) {
        return Err("size must be between 16x16 and 8192x8192".into());
    }
    Ok((w, h))
}
