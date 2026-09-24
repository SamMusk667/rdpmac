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
}
