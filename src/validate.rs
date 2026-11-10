//! Pure checks for names and paths that cross a trust boundary
//! (D-Bus options from apps, input typed by the user, results from the picker).

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

const NAME_MAX: usize = 255;

/// Decodes a portal `ay` path option (NUL-terminated by convention).
pub fn path_from_bytes(bytes: &[u8]) -> Option<PathBuf> {
    let bytes = bytes.strip_suffix(&[0]).unwrap_or(bytes);
    if bytes.is_empty() || bytes.contains(&0) {
        return None;
    }
    Some(PathBuf::from(OsStr::from_bytes(bytes)))
}

/// A single path component that is safe to join onto a directory.
pub fn is_valid_filename(name: &OsStr) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= NAME_MAX
        && !bytes.contains(&b'/')
        && !bytes.contains(&0)
        && bytes != b"."
        && bytes != b".."
}

/// A valid filename that is also safe to print in a terminal. Required for every
/// name that is shown before the user has approved it (app suggestions, typed input).
pub fn is_plain_filename(name: &OsStr) -> bool {
    is_valid_filename(name) && is_plain_text(&name.to_string_lossy())
}

/// No characters that can drive the terminal or disguise what is shown.
pub fn is_plain_text(text: &str) -> bool {
    !text.chars().any(is_deceptive_char)
}

/// C0/C1 controls and DEL (escape sequences), plus bidi overrides/isolates,
/// zero-width characters and line/paragraph separators (spoofed names).
pub fn is_deceptive_char(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{061C}'
                | '\u{200B}'..='\u{200F}'
                | '\u{2028}'..='\u{202E}'
                | '\u{2060}'..='\u{2069}'
                | '\u{FEFF}'
        )
}

/// Reduces an app-suggested name to its final component, never a path.
pub fn suggested_filename(raw: &OsStr) -> Option<OsString> {
    let name = Path::new(raw).file_name()?;
    is_plain_filename(name).then(|| name.to_owned())
}

/// Splits a filename so an editor cursor can start before the extension.
pub fn split_extension(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(0) | None => (name, ""),
        Some(i) => name.split_at(i),
    }
}

pub fn file_uri(path: &Path) -> Option<String> {
    url::Url::from_file_path(path).ok().map(String::from)
}

/// Parent directory of an absolute save target, if it currently is a directory.
pub fn existing_parent(path: &Path) -> Option<&Path> {
    if !path.is_absolute() {
        return None;
    }
    let parent = path.parent()?;
    let name = path.file_name()?;
    (is_valid_filename(name) && parent.is_dir()).then_some(parent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_from_bytes_strips_terminator_and_rejects_embedded_nul() {
        assert_eq!(
            path_from_bytes(b"/home/u\0"),
            Some(PathBuf::from("/home/u"))
        );
        assert_eq!(path_from_bytes(b"/home/u"), Some(PathBuf::from("/home/u")));
        assert_eq!(path_from_bytes(b"/a\0b\0"), None);
        assert_eq!(path_from_bytes(b"\0"), None);
    }

    #[test]
    fn filenames_cannot_escape_the_directory() {
        for bad in ["", ".", "..", "a/b", "../x", "a\0b"] {
            assert!(!is_valid_filename(OsStr::new(bad)), "{bad:?}");
        }
        assert!(!is_valid_filename(OsStr::new(&"x".repeat(256))));
        assert!(is_valid_filename(OsStr::new("report.pdf")));
        assert!(is_valid_filename(OsStr::new("..hidden")));
    }

    #[test]
    fn suggested_filename_keeps_only_last_component() {
        let name = |s: &str| suggested_filename(OsStr::new(s));
        assert_eq!(name("../../.bashrc"), Some(".bashrc".into()));
        assert_eq!(name("/etc/passwd"), Some("passwd".into()));
        assert_eq!(name(".."), None);
        assert_eq!(name(""), None);
    }

    #[test]
    fn suggested_filename_rejects_terminal_and_bidi_tricks() {
        let name = |s: &str| suggested_filename(OsStr::new(s));
        for evil in [
            "a\x1b]0;pwned\x07.txt",
            "\x1b[2J.txt",
            "a\u{9b}31m.txt",
            "a\x7f.txt",
            "invoice\u{202E}fdp.exe",
            "a\u{2066}b",
            "a\u{200B}b",
            "a\u{2028}b",
        ] {
            assert_eq!(name(evil), None, "{evil:?}");
        }
        assert_eq!(name("résumé 2.pdf"), Some("résumé 2.pdf".into()));
    }

    #[test]
    fn split_extension_keeps_dotfiles_whole() {
        assert_eq!(split_extension("a.tar.gz"), ("a.tar", ".gz"));
        assert_eq!(split_extension(".bashrc"), (".bashrc", ""));
        assert_eq!(split_extension("README"), ("README", ""));
    }

    #[test]
    fn file_uri_percent_encodes_non_utf8_and_specials() {
        let path = PathBuf::from(OsStr::from_bytes(b"/tmp/a b#\xff"));
        assert_eq!(file_uri(&path).unwrap(), "file:///tmp/a%20b%23%FF");
        assert_eq!(file_uri(Path::new("relative")), None);
    }

    #[test]
    fn existing_parent_requires_absolute_path_in_real_dir() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("new.txt");
        assert_eq!(existing_parent(&target), Some(dir.path()));
        assert_eq!(existing_parent(Path::new("new.txt")), None);
        assert_eq!(existing_parent(&dir.path().join("missing/new.txt")), None);
        assert_eq!(existing_parent(Path::new("/")), None);
    }
}
