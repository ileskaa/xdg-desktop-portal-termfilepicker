//! What the daemon asks the picker to do, and what the picker answers.
//! Both travel as files inside the private workspace (never argv), encoded as
//! NUL-separated byte strings so any Unix path round-trips.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

pub const REQUEST_FILE: &str = "request";
pub const RESULT_FILE: &str = "result";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Open { multiple: bool, directory: bool },
    Save { name: OsString },
    SaveMany { names: Vec<OsString> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickRequest {
    pub mode: Mode,
    pub start_dir: PathBuf,
    /// Who asks, as reported by xdg-desktop-portal; empty for unsandboxed apps.
    pub app_id: String,
    /// Dialog title chosen by the app.
    pub title: String,
}

impl PickRequest {
    pub fn encode(&self) -> Vec<u8> {
        let flag = |b: bool| OsString::from(if b { "1" } else { "0" });
        let mut fields: Vec<OsString> = Vec::new();
        match &self.mode {
            Mode::Open {
                multiple,
                directory,
            } => fields.extend(["open".into(), flag(*multiple), flag(*directory)]),
            Mode::Save { name } => fields.extend(["save".into(), name.clone()]),
            Mode::SaveMany { names } => {
                fields.push("save-many".into());
                fields.extend(names.iter().cloned());
            }
        }
        let header = [
            self.start_dir.clone().into_os_string(),
            self.app_id.clone().into(),
            self.title.clone().into(),
        ];
        fields.splice(0..0, header);
        join(&fields)
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let fields = split(bytes);
        let [start_dir, app_id, title, kind, args @ ..] = fields.as_slice() else {
            return None;
        };
        let start_dir = PathBuf::from(start_dir);
        let app_id = app_id.to_str()?.to_owned();
        let title = title.to_str()?.to_owned();
        let flag = |f: &OsString| match f.as_bytes() {
            b"1" => Some(true),
            b"0" => Some(false),
            _ => None,
        };
        let mode = match (kind.as_bytes(), args) {
            (b"open", [m, d]) => Mode::Open {
                multiple: flag(m)?,
                directory: flag(d)?,
            },
            (b"save", [name]) => Mode::Save { name: name.clone() },
            (b"save-many", names) if !names.is_empty() => Mode::SaveMany {
                names: names.to_vec(),
            },
            _ => return None,
        };
        start_dir.is_absolute().then_some(Self {
            mode,
            start_dir,
            app_id,
            title,
        })
    }
}

pub fn encode_paths(paths: &[PathBuf]) -> Vec<u8> {
    let fields: Vec<OsString> = paths.iter().map(|p| p.clone().into_os_string()).collect();
    join(&fields)
}

pub fn decode_paths(bytes: &[u8]) -> Vec<PathBuf> {
    split(bytes).into_iter().map(PathBuf::from).collect()
}

fn join(fields: &[OsString]) -> Vec<u8> {
    fields
        .iter()
        .map(|f| f.as_bytes())
        .collect::<Vec<_>>()
        .join(&0u8)
}

fn split(bytes: &[u8]) -> Vec<OsString> {
    if bytes.is_empty() {
        return Vec::new();
    }
    bytes
        .split(|b| *b == 0)
        .map(|f| OsStr::from_bytes(f).to_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(mode: Mode) {
        let req = PickRequest {
            mode,
            start_dir: PathBuf::from(OsStr::from_bytes(b"/home/u/d\xffir")),
            app_id: "org.example.App".into(),
            title: "Upload".into(),
        };
        assert_eq!(PickRequest::decode(&req.encode()), Some(req));
    }

    #[test]
    fn requests_round_trip() {
        round_trip(Mode::Open {
            multiple: true,
            directory: false,
        });
        round_trip(Mode::Open {
            multiple: false,
            directory: true,
        });
        round_trip(Mode::Save {
            name: "a\nb.txt".into(),
        });
        round_trip(Mode::SaveMany {
            names: vec!["a".into(), "b".into()],
        });
    }

    #[test]
    fn malformed_requests_are_rejected() {
        assert_eq!(PickRequest::decode(b""), None);
        assert_eq!(PickRequest::decode(b"relative\0a\0t\0open\x000\x000"), None);
        assert_eq!(PickRequest::decode(b"/d\0a\0t\0open\x002\x000"), None);
        assert_eq!(PickRequest::decode(b"/d\0a\0t\0save-many"), None);
        assert_eq!(PickRequest::decode(b"/d\0a\0t\0format\0x"), None);
        assert_eq!(PickRequest::decode(b"/d\0open\x000\x000"), None);
    }

    #[test]
    fn paths_round_trip() {
        let paths = vec![PathBuf::from("/a b"), PathBuf::from("/c\nd")];
        assert_eq!(decode_paths(&encode_paths(&paths)), paths);
        assert!(decode_paths(b"").is_empty());
    }
}
