//! Resolves which terminal emulator hosts the picker and how to invoke it so
//! that it blocks until the picker exits. No shell is ever involved.

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::config::TerminalSetting;

pub const APP_ID: &str = "termfilepicker";

/// Argv prefixes; the picker command is appended. Flags force a fresh, blocking
/// process where the terminal would otherwise hand off to a running instance.
const PRESETS: &[(&str, &[&str])] = &[
    ("alacritty", &["alacritty", "--class", APP_ID, "-e"]),
    ("foot", &["foot", "--app-id", APP_ID, "--"]),
    ("kitty", &["kitty", "--class", APP_ID]),
    (
        "wezterm",
        &[
            "wezterm",
            "start",
            "--always-new-process",
            "--class",
            APP_ID,
            "--",
        ],
    ),
    ("ghostty", &["ghostty", "--gtk-single-instance=false", "-e"]),
];

pub fn preset_names() -> impl Iterator<Item = &'static str> {
    PRESETS.iter().map(|(name, _)| *name)
}

/// Explicit setting, else `$TERMINAL` if it names a preset, else the first preset on `PATH`.
pub fn resolve(setting: Option<&TerminalSetting>) -> Result<Vec<OsString>, String> {
    match setting {
        Some(TerminalSetting::Command(argv)) if !argv.is_empty() => {
            Ok(argv.iter().map(OsString::from).collect())
        }
        Some(TerminalSetting::Command(_)) => Err("terminal command is empty".into()),
        Some(TerminalSetting::Preset(name)) => {
            preset(name).ok_or_else(|| format!("unknown terminal preset {name:?}"))
        }
        None => {
            detect().ok_or_else(|| "no supported terminal found; set `terminal` in config".into())
        }
    }
}

fn preset(name: &str) -> Option<Vec<OsString>> {
    let (_, argv) = PRESETS.iter().find(|(n, _)| *n == name)?;
    Some(argv.iter().map(OsString::from).collect())
}

fn detect() -> Option<Vec<OsString>> {
    let from_env = std::env::var_os("TERMINAL")
        .and_then(|t| {
            Path::new(&t)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
        })
        .and_then(|name| preset(&name));
    from_env.or_else(|| {
        preset_names()
            .find(|n| find_in_path(n).is_some())
            .and_then(preset)
    })
}

pub fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|p| is_executable(p))
}

fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_resolve_and_unknown_names_fail() {
        let argv = resolve(Some(&TerminalSetting::Preset("alacritty".into()))).unwrap();
        assert_eq!(argv.last().unwrap(), "-e");
        assert!(resolve(Some(&TerminalSetting::Preset("xterm; rm -rf ~".into()))).is_err());
        assert!(resolve(Some(&TerminalSetting::Command(vec![]))).is_err());
    }

    #[test]
    fn custom_command_is_used_verbatim() {
        let argv = resolve(Some(&TerminalSetting::Command(vec![
            "t".into(),
            "-x".into(),
        ])))
        .unwrap();
        assert_eq!(argv, ["t", "-x"]);
    }
}
