//! Optional user config at `$XDG_CONFIG_HOME/xdg-desktop-portal-termfilepicker/config.toml`.
//! Re-read on every request so edits apply without restarting the service.

use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Preset name (`"alacritty"`, `"foot"`, …) or a full argv prefix that runs
    /// the remaining arguments as a command and blocks until it exits.
    pub terminal: Option<TerminalSetting>,
    /// Yazi executable; defaults to `yazi` from `PATH`.
    pub yazi: Option<PathBuf>,
    /// Where to start when the app suggests no folder; `~/` is expanded.
    pub default_dir: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum TerminalSetting {
    Preset(String),
    Command(Vec<String>),
}

impl Config {
    pub fn load() -> Result<Self, String> {
        let Some(path) = config_path() else {
            return Ok(Self::default());
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::parse(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("cannot read config: {e}")),
        }
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        toml::from_str(text).map_err(|e| format!("invalid config: {e}"))
    }

    pub fn yazi(&self) -> PathBuf {
        self.yazi.clone().unwrap_or_else(|| PathBuf::from("yazi"))
    }

    pub fn default_dir(&self) -> Option<PathBuf> {
        let raw = self.default_dir.as_deref()?;
        match raw.strip_prefix("~/") {
            Some(rest) => home_dir().map(|h| h.join(rest)),
            None if raw == "~" => home_dir(),
            None => Some(PathBuf::from(raw)),
        }
    }
}

pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

fn config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| home_dir().map(|h| h.join(".config")))?;
    Some(base.join("xdg-desktop-portal-termfilepicker/config.toml"))
}

/// Start folder: the app's suggestion, else the configured default, else `$HOME`.
pub fn start_dir(requested: Option<&Path>, config: &Config) -> PathBuf {
    requested
        .map(Path::to_path_buf)
        .filter(|p| p.is_absolute() && p.is_dir())
        .or_else(|| config.default_dir().filter(|p| p.is_dir()))
        .or_else(home_dir)
        .unwrap_or_else(|| PathBuf::from("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_preset_and_custom_terminal() {
        let c = Config::parse("terminal = \"foot\"").unwrap();
        assert_eq!(c.terminal, Some(TerminalSetting::Preset("foot".into())));
        let c = Config::parse("terminal = [\"foot\", \"-a\", \"x\", \"--\"]").unwrap();
        assert!(matches!(c.terminal, Some(TerminalSetting::Command(v)) if v.len() == 4));
    }

    #[test]
    fn rejects_unknown_keys() {
        assert!(Config::parse("cmd = \"sh -c evil\"").is_err());
    }

    #[test]
    fn start_dir_ignores_missing_or_relative_suggestions() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::default();
        assert_eq!(start_dir(Some(dir.path()), &config), dir.path());
        let fallback = start_dir(Some(Path::new("relative")), &config);
        assert!(fallback.is_absolute());
        assert_ne!(
            start_dir(Some(&dir.path().join("missing")), &config),
            dir.path().join("missing")
        );
    }
}
