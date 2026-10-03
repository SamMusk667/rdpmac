use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Parser, ValueEnum};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthMode {
    /// Local and directory accounts through PAM.
    Pam,
    /// One fixed username and password, for development.
    Static,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Security {
    /// TLS: the client sends the password inside the encrypted connection and the account check
    /// happens once the session is set up.
    Tls,
    /// Network Level Authentication: the client proves it knows the password before a session
    /// exists and hands it over only once rdpmacd proved it knows the account too. Only accounts
    /// enrolled for it can log on, and clients without NLA are turned away.
    Nla,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Resolution {
    /// The size the client asks for (mstsc /w /h, full screen, window resizing); the display is
    /// scaled to it when the two differ.
    FollowClient,
    /// The display's own pixel size.
    Native,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VirtualDisplay {
    /// On a Mac without a screen attached, a session following the client gets its own display
    /// at the client's size instead of a scaled picture.
    Auto,
    /// Never create a display; scale the existing one.
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Codec {
    /// H.264 through the graphics pipeline when the client supports it, in full colour (AVC444)
    /// where it can; RemoteFX otherwise.
    Auto,
    /// H.264 in 4:2:0 colour (AVC420) only, which costs less to encode and send; RemoteFX when
    /// the client has no H.264.
    Avc420,
    /// RemoteFX and bitmap updates only.
    Remotefx,
}

#[derive(Debug, Parser)]
#[command(name = "rdpmacd", version = env!("RDPMAC_VERSION"), about = "RDP server for the macOS console session")]
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

    /// Convert colours for AVC444 on several cores.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    pub parallel_conversion: bool,

    /// Do not share the clipboard with clients.
    #[arg(long)]
    pub no_clipboard: bool,

    /// Do not play the Mac's sound on clients.
    #[arg(long)]
    pub no_audio: bool,

    /// Do not mount the drives clients share in ~/RDP Drives (drive redirection).
    #[arg(long)]
    pub no_drives: bool,

    /// Do not type the password of the user who logs on into the Mac's lock screen.
    #[arg(long)]
    pub no_unlock: bool,

    /// Mute the Mac's own sound output while a client plays the sound, as Windows does.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    pub mute_mac: bool,

    /// Sample rate offered first for the sound, 44100 or 48000. At 48000 mstsc plays slower than
    /// real time and falls ever further behind.
    #[arg(long, default_value_t = 44_100, value_parser = parse_rate)]
    pub audio_rate: u32,

    /// Record the H.264 stream exactly as sent, with the client's acknowledgements, under h264/
    /// in the log directory, to find out afterwards why a client showed a wrong picture. Keeps
    /// up to about 3 GB.
    #[arg(long)]
    pub h264_dump: bool,

    /// How the session resolution is chosen.
    #[arg(long, value_enum, default_value_t = Resolution::FollowClient)]
    pub resolution: Resolution,

    /// Whether sessions that follow the client may get a display of their own.
    #[arg(long, value_enum, default_value_t = VirtualDisplay::Auto)]
    pub virtual_display: VirtualDisplay,

    #[arg(long, value_enum, default_value_t = AuthMode::Pam)]
    pub auth: AuthMode,

    /// How clients authenticate.
    #[arg(long, value_enum, default_value_t = Security::Tls)]
    pub security: Security,

    /// PAM service to authenticate against.
    #[arg(long, default_value = "checkpw")]
    pub pam_service: String,

    /// Another account that may log on, besides the user rdpmacd runs as, who always may. It takes
    /// over that user's console session. Repeat the flag for more accounts.
    #[arg(long = "allow-user", value_name = "NAME")]
    pub allow_users: Vec<String>,

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

    /// Settings file; flags given on the command line override it. Defaults to config.toml in
    /// the data directory.
    #[arg(long)]
    pub config: Option<PathBuf>,

    /// Serve a synthetic moving picture of this size (for example 1920x1080) instead of the
    /// screen. Needs no permission; used to test clients and measure encoding cost.
    #[arg(long, value_parser = parse_size)]
    pub test_pattern: Option<(u32, u32)>,

    /// Trigger the macOS permission prompts for screen recording and accessibility, then exit.
    #[arg(long)]
    pub request_permissions: bool,

    /// Switch display ID to its WIDTHxHEIGHT mode, then exit. rdpmacd runs itself with this for
    /// its virtual display, because the switch has to come from another process.
    #[arg(long, hide = true, value_name = "ID:WIDTHxHEIGHT", value_parser = parse_switch)]
    pub switch_display_mode: Option<(u32, u32, u32)>,
}

fn parse_switch(text: &str) -> Result<(u32, u32, u32), String> {
    let (id, size) = text
        .split_once(':')
        .ok_or_else(|| format!("expected ID:WIDTHxHEIGHT, got {text}"))?;
    let id = id.trim().parse::<u32>().map_err(|e| format!("{id}: {e}"))?;
    let (width, height) = parse_size(size)?;
    Ok((id, width, height))
}

pub fn parse_rate(text: &str) -> Result<u32, String> {
    match text.trim().parse::<u32>() {
        Ok(rate @ (44_100 | 48_000)) => Ok(rate),
        _ => Err(format!("expected 48000 or 44100, got {text}")),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switch_argument_names_a_display_and_a_size() {
        assert_eq!(parse_switch("7:3840x2160"), Ok((7, 3840, 2160)));
        assert!(parse_switch("3840x2160").is_err());
        assert!(parse_switch("x:3840x2160").is_err());
    }
}
