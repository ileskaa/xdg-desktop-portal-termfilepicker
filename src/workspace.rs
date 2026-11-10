//! Private per-request scratch directory shared by the daemon, the picker and Yazi.
//!
//! Every directory is created with a random name and mode 0700, files are 0600,
//! and reads refuse symlinks, foreign owners and oversized content. Nothing here
//! ever lives at a predictable path in a world-writable directory.

use std::fs::{self, DirBuilder, File, OpenOptions, Permissions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use tempfile::TempDir;

const BASE_NAME: &str = "xdg-desktop-portal-termfilepicker";
pub const MAX_FILE_SIZE: u64 = 1 << 20;

pub struct Workspace {
    dir: TempDir,
}

impl Workspace {
    pub fn create() -> io::Result<Self> {
        let dir = tempfile::Builder::new()
            .prefix("req-")
            .permissions(Permissions::from_mode(0o700))
            .tempdir_in(base_dir()?)?;
        Ok(Self { dir })
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }
}

/// `$XDG_RUNTIME_DIR/xdg-desktop-portal-termfilepicker`, created 0700 and verified to be ours.
pub fn base_dir() -> io::Result<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "XDG_RUNTIME_DIR unset"))?;
    ensure_private_dir(&runtime)?;
    let base = runtime.join(BASE_NAME);
    match DirBuilder::new().mode(0o700).create(&base) {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
        _ => {}
    }
    ensure_private_dir(&base)?;
    Ok(base)
}

/// Removes workspaces left behind by a previous daemon that was killed mid-request.
pub fn remove_stale() -> io::Result<()> {
    for entry in fs::read_dir(base_dir()?)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            fs::remove_dir_all(entry.path())?;
        }
    }
    Ok(())
}

fn ensure_private_dir(path: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    let ours = meta.is_dir() && meta.uid() == current_uid() && meta.mode() & 0o077 == 0;
    if !ours {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "directory is not private to this user",
        ));
    }
    Ok(())
}

fn current_uid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

/// Reads `dir/name`; `Ok(None)` if absent. Refuses symlinks, non-regular files,
/// files not owned by us and anything larger than [`MAX_FILE_SIZE`].
pub fn read_private(dir: &Path, name: &str) -> io::Result<Option<Vec<u8>>> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(dir.join(name))
    {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let meta = file.metadata()?;
    if !meta.is_file() || meta.uid() != current_uid() || meta.len() > MAX_FILE_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "refusing untrusted workspace file",
        ));
    }
    let mut buf = Vec::new();
    file.take(MAX_FILE_SIZE + 1).read_to_end(&mut buf)?;
    if buf.len() as u64 > MAX_FILE_SIZE {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "file too large"));
    }
    Ok(Some(buf))
}

/// Atomically publishes `dir/name` (exclusive 0600 temp file, then rename).
pub fn write_private(dir: &Path, name: &str, contents: &[u8]) -> io::Result<()> {
    let tmp = dir.join(format!(".{name}.tmp"));
    remove_if_exists(&tmp)?;
    let mut file: File = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&tmp)?;
    file.write_all(contents)?;
    file.sync_all()?;
    fs::rename(&tmp, dir.join(name))
}

pub fn remove_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn private_tempdir() -> TempDir {
        tempfile::Builder::new()
            .permissions(Permissions::from_mode(0o700))
            .tempdir()
            .unwrap()
    }

    #[test]
    fn workspaces_are_random_and_private() {
        let a = Workspace::create().unwrap();
        let b = Workspace::create().unwrap();
        assert_ne!(a.path(), b.path());
        let mode = fs::metadata(a.path()).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o700);
        let kept = a.path().to_owned();
        drop(a);
        assert!(!kept.exists(), "workspace must be removed on drop");
    }

    #[test]
    fn written_files_are_owner_only_and_round_trip() {
        let dir = private_tempdir();
        write_private(dir.path(), "result", b"/a\0/b").unwrap();
        write_private(dir.path(), "result", b"/c").unwrap();
        let mode = fs::metadata(dir.path().join("result")).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(read_private(dir.path(), "result").unwrap().unwrap(), b"/c");
    }

    #[test]
    fn read_refuses_symlinks_fifos_and_oversized_files() {
        let dir = private_tempdir();
        let secret = dir.path().join("secret");
        fs::write(&secret, b"x").unwrap();
        symlink(&secret, dir.path().join("link")).unwrap();
        assert!(read_private(dir.path(), "link").is_err());

        let fifo = std::ffi::CString::new(
            dir.path()
                .join("fifo")
                .into_os_string()
                .into_encoded_bytes(),
        )
        .unwrap();
        // SAFETY: valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert!(
            read_private(dir.path(), "fifo").is_err(),
            "must not block on or accept a fifo"
        );

        fs::write(
            dir.path().join("big"),
            vec![b'a'; MAX_FILE_SIZE as usize + 1],
        )
        .unwrap();
        assert!(read_private(dir.path(), "big").is_err());
        assert_eq!(read_private(dir.path(), "absent").unwrap(), None);
    }

    #[test]
    fn write_does_not_follow_a_planted_temp_symlink() {
        let dir = private_tempdir();
        let victim = dir.path().join("victim");
        fs::write(&victim, b"keep").unwrap();
        symlink(&victim, dir.path().join(".result.tmp")).unwrap();
        write_private(dir.path(), "result", b"new").unwrap();
        assert_eq!(fs::read(&victim).unwrap(), b"keep");
    }

    #[test]
    fn private_dir_check_rejects_group_access_and_symlinks() {
        let dir = private_tempdir();
        assert!(ensure_private_dir(dir.path()).is_ok());
        let open = dir.path().join("open");
        DirBuilder::new().mode(0o755).create(&open).unwrap();
        fs::set_permissions(&open, Permissions::from_mode(0o755)).unwrap();
        assert!(ensure_private_dir(&open).is_err());
        let link = dir.path().join("link");
        symlink(dir.path(), &link).unwrap();
        assert!(ensure_private_dir(&link).is_err());
    }
}
