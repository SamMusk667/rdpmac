//! The settings file, `config.toml` in the data directory. A value given on the command line wins
//! over the file; the menu-bar app edits the file through the control socket.

use std::fs;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{ensure, Context};
use clap::parser::ValueSource;
use clap::ArgMatches;
use serde::{Deserialize, Serialize};

use crate::config::{Args, AuthMode, Codec, Resolution, Security, VirtualDisplay};

const HEADER: &str = "# rdpmacd settings. Flags on the command line override them; restart rdpmacd to apply.\n\n";

/// Every field is optional: one that is absent keeps the built-in default.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct Settings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub listen: Option<SocketAddr>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub security: Option<Security>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pam_service: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codec: Option<Codec>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clipboard: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolution: Option<Resolution>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub virtual_display: Option<VirtualDisplay>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fps: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor_hz: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cert: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<PathBuf>,
}

impl Settings {
    /// An absent file means no settings.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let settings: Self = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        settings.validate().with_context(|| format!("checking {}", path.display()))?;
        Ok(settings)
    }

    /// Rejects what the flag parsers would reject.
    pub fn validate(&self) -> anyhow::Result<()> {
        if let Some(fps) = self.fps {
            ensure!((1..=120).contains(&fps), "fps must be between 1 and 120");
        }
        if let Some(hz) = self.cursor_hz {
            ensure!((1..=120).contains(&hz), "cursor-hz must be between 1 and 120");
        }
        if let Some(service) = &self.pam_service {
            ensure!(!service.trim().is_empty(), "pam-service must not be empty");
        }
        ensure!(
            self.cert.is_some() == self.key.is_some(),
            "cert and key must be set together"
        );
        Ok(())
    }

    /// Replaces the file atomically; it is readable only by the user.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        self.validate()?;
        let text = toml::to_string_pretty(self).context("encoding the settings")?;
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let staged = path.with_extension("toml.new");
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&staged)
            .with_context(|| format!("writing {}", staged.display()))?;
        file.write_all(HEADER.as_bytes())?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        fs::rename(&staged, path).with_context(|| format!("replacing {}", path.display()))
    }

    /// Fills `args` from the settings wherever the command line did not set a value.
    pub fn apply(&self, args: &mut Args, matches: &ArgMatches) {
        let unset = |id: &str| matches.value_source(id) != Some(ValueSource::CommandLine);
        if let (Some(v), true) = (self.listen, unset("listen")) {
            args.listen = v;
        }
        if let (Some(v), true) = (self.auth, unset("auth")) {
            args.auth = v;
        }
        if let (Some(v), true) = (self.security, unset("security")) {
            args.security = v;
        }
        if let (Some(v), true) = (&self.pam_service, unset("pam_service")) {
            args.pam_service = v.clone();
        }
        if let (Some(v), true) = (self.codec, unset("codec")) {
            args.codec = v;
        }
        if let (Some(v), true) = (self.clipboard, unset("no_clipboard")) {
            args.no_clipboard = !v;
        }
        if let (Some(v), true) = (self.resolution, unset("resolution")) {
            args.resolution = v;
        }
        if let (Some(v), true) = (self.virtual_display, unset("virtual_display")) {
            args.virtual_display = v;
        }
        if let (Some(v), true) = (self.fps, unset("fps")) {
            args.fps = v;
        }
        if let (Some(v), true) = (self.cursor_hz, unset("cursor_hz")) {
            args.cursor_hz = v;
        }
        if unset("cert") && unset("key") {
            if let (Some(cert), Some(key)) = (&self.cert, &self.key) {
                args.cert = Some(cert.clone());
                args.key = Some(key.clone());
            }
        }
    }

    /// The settings in effect after flags and file are combined.
    pub fn effective(args: &Args) -> Self {
        Self {
            listen: Some(args.listen),
            auth: Some(args.auth),
            security: Some(args.security),
            pam_service: Some(args.pam_service.clone()),
            codec: Some(args.codec),
            clipboard: Some(!args.no_clipboard),
            resolution: Some(args.resolution),
            virtual_display: Some(args.virtual_display),
            fps: Some(args.fps),
            cursor_hz: Some(args.cursor_hz),
            cert: args.cert.clone(),
            key: args.key.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, FromArgMatches};

    use super::*;

    fn parse(argv: &[&str]) -> (Args, ArgMatches) {
        let matches = Args::command()
            .try_get_matches_from(std::iter::once("rdpmacd").chain(argv.iter().copied()))
            .expect("valid flags");
        let args = Args::from_arg_matches(&matches).expect("args from matches");
        (args, matches)
    }

    fn sample() -> Settings {
        toml::from_str(
            r#"
            listen = "127.0.0.1:4000"
            security = "nla"
            clipboard = false
            resolution = "native"
            virtual-display = "off"
            fps = 25
            "#,
        )
        .expect("valid settings")
    }

    #[test]
    fn file_fills_what_the_command_line_leaves_unset() {
        let (mut args, matches) = parse(&["--fps", "60"]);
        sample().apply(&mut args, &matches);
        assert_eq!(args.listen, "127.0.0.1:4000".parse().expect("address"));
        assert_eq!(args.security, Security::Nla);
        assert!(args.no_clipboard);
        assert_eq!(args.resolution, Resolution::Native);
        assert_eq!(args.virtual_display, VirtualDisplay::Off);
        assert_eq!(args.fps, 60, "the flag wins");
        assert_eq!(args.codec, Codec::Auto, "unset in both keeps the default");
    }

    #[test]
    fn unknown_keys_and_bad_values_are_rejected() {
        assert!(toml::from_str::<Settings>("lisen = \"0.0.0.0:3389\"").is_err());
        assert!(toml::from_str::<Settings>("codec = \"h265\"").is_err());
        let settings: Settings = toml::from_str("codec = \"avc420\"").expect("parses");
        assert_eq!(settings.codec, Some(Codec::Avc420));
        let settings: Settings = toml::from_str("fps = 500").expect("parses");
        assert!(settings.validate().is_err());
        let settings: Settings = toml::from_str("cert = \"/tmp/c.pem\"").expect("parses");
        assert!(settings.validate().is_err(), "cert without key");
    }

    #[test]
    fn saved_file_reads_back() {
        let dir = std::env::temp_dir().join(format!("rdpmacd-settings-{}", std::process::id()));
        let path = dir.join("config.toml");
        let settings = sample();
        settings.save(&path).expect("saved");
        assert_eq!(Settings::load(&path).expect("loaded"), settings);
        let mode = fs::metadata(&path).expect("metadata");
        assert_eq!(std::os::unix::fs::PermissionsExt::mode(&mode.permissions()) & 0o777, 0o600);
        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn missing_file_is_empty() {
        let path = std::env::temp_dir().join("rdpmacd-no-such-settings.toml");
        assert_eq!(Settings::load(&path).expect("loaded"), Settings::default());
    }
}
