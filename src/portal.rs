//! D-Bus side: `org.freedesktop.impl.portal.FileChooser` and per-request
//! `org.freedesktop.impl.portal.Request` objects.
//!
//! Logs carry request handles, modes and counts only — never paths or names.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::process::{Child, Command};
use tokio::sync::{Mutex, Semaphore, oneshot};
use tokio::time::timeout;
use tracing::{info, warn};
use zbus::message::Header;
use zbus::names::BusName;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, fdo, interface};

use crate::config::{self, Config};
use crate::protocol::{self, Mode, PickRequest, REQUEST_FILE, RESULT_FILE};
use crate::terminal;
use crate::validate::{existing_parent, file_uri, path_from_bytes, suggested_filename};
use crate::workspace::{MAX_FILE_SIZE, Workspace, read_private, write_private};

pub const BUS_NAME: &str = "org.freedesktop.impl.portal.desktop.termfilepicker";
pub const OBJECT_PATH: &str = "/org/freedesktop/portal/desktop";
const FRONTEND_NAME: &str = "org.freedesktop.portal.Desktop";
const REQUEST_PREFIX: &str = "/org/freedesktop/portal/desktop/request/";
const MAX_RESULTS: usize = 10_000;
const MAX_CONCURRENT: usize = 4;
const MAX_TITLE_CHARS: usize = 200;
const STOP_GRACE: Duration = Duration::from_secs(2);

const RESPONSE_SUCCESS: u32 = 0;
const RESPONSE_CANCELLED: u32 = 1;
const RESPONSE_OTHER: u32 = 2;

type Options = HashMap<String, OwnedValue>;
type Response = (u32, HashMap<String, OwnedValue>);

pub struct FileChooser {
    exe: PathBuf,
    slots: Arc<Semaphore>,
}

// Arity is fixed by the portal D-Bus signature.
#[allow(clippy::too_many_arguments)]
#[interface(name = "org.freedesktop.impl.portal.FileChooser")]
impl FileChooser {
    #[zbus(out_args("response", "results"))]
    async fn open_file(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
        handle: OwnedObjectPath,
        app_id: String,
        _parent_window: String,
        title: String,
        options: Options,
    ) -> fdo::Result<Response> {
        ensure_frontend(conn, &header, &handle).await?;
        let mode = Mode::Open {
            multiple: opt_bool(&options, "multiple"),
            directory: opt_bool(&options, "directory"),
        };
        let folder = opt_path(&options, "current_folder");
        Ok(self
            .run(
                conn,
                handle,
                Caller::new(app_id, title),
                mode,
                folder.as_deref(),
            )
            .await)
    }

    #[zbus(out_args("response", "results"))]
    async fn save_file(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
        handle: OwnedObjectPath,
        app_id: String,
        _parent_window: String,
        title: String,
        options: Options,
    ) -> fdo::Result<Response> {
        ensure_frontend(conn, &header, &handle).await?;
        let current_file = opt_path(&options, "current_file");
        let name = current_file
            .as_deref()
            .and_then(Path::file_name)
            .map(OsStr::to_owned)
            .or_else(|| opt_str(&options, "current_name").map(OsString::from))
            .and_then(|n| suggested_filename(&n))
            .unwrap_or_else(|| OsString::from("untitled"));
        let folder = current_file
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_owned)
            .or_else(|| opt_path(&options, "current_folder"));
        let mode = Mode::Save { name };
        Ok(self
            .run(
                conn,
                handle,
                Caller::new(app_id, title),
                mode,
                folder.as_deref(),
            )
            .await)
    }

    #[zbus(out_args("response", "results"))]
    async fn save_files(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
        handle: OwnedObjectPath,
        app_id: String,
        _parent_window: String,
        title: String,
        options: Options,
    ) -> fdo::Result<Response> {
        ensure_frontend(conn, &header, &handle).await?;
        let raw = opt_byte_strings(&options, "files").unwrap_or_default();
        let names: Option<Vec<OsString>> = raw.iter().map(|n| name_from_bytes(n)).collect();
        let Some(names) = names.filter(|n| !n.is_empty() && n.len() <= MAX_RESULTS) else {
            return Err(fdo::Error::InvalidArgs(
                "`files` must list valid file names".into(),
            ));
        };
        let folder = opt_path(&options, "current_folder");
        let mode = Mode::SaveMany { names };
        Ok(self
            .run(
                conn,
                handle,
                Caller::new(app_id, title),
                mode,
                folder.as_deref(),
            )
            .await)
    }
}

impl FileChooser {
    pub fn new(exe: PathBuf) -> Self {
        Self {
            exe,
            slots: Arc::new(Semaphore::new(MAX_CONCURRENT)),
        }
    }

    async fn run(
        &self,
        conn: &Connection,
        handle: OwnedObjectPath,
        caller: Caller,
        mode: Mode,
        folder: Option<&Path>,
    ) -> Response {
        let kind = mode_name(&mode);
        info!(handle = %handle.as_str(), app_id = %caller.app_id, kind, "request");
        match self.track(conn, &handle, caller, mode, folder).await {
            Ok(Some(uris)) => {
                info!(handle = %handle.as_str(), count = uris.len(), "chosen");
                success(uris)
            }
            Ok(None) => {
                info!(handle = %handle.as_str(), "cancelled");
                (RESPONSE_CANCELLED, HashMap::new())
            }
            Err(e) => {
                warn!(handle = %handle.as_str(), error = %e, "failed");
                (RESPONSE_OTHER, HashMap::new())
            }
        }
    }

    /// Exports the Request object first so a `Close` can never miss, and removes
    /// it afterwards only if this call exported it.
    async fn track(
        &self,
        conn: &Connection,
        handle: &OwnedObjectPath,
        caller: Caller,
        mode: Mode,
        folder: Option<&Path>,
    ) -> Result<Option<Vec<String>>, String> {
        let _slot = Arc::clone(&self.slots)
            .try_acquire_owned()
            .map_err(|_| "too many open pickers")?;
        let (close_tx, close_rx) = oneshot::channel();
        let request = Request {
            close: Mutex::new(Some(close_tx)),
        };
        let exported = conn
            .object_server()
            .at(handle.as_ref(), request)
            .await
            .map_err(|e| format!("cannot export request: {e}"))?;
        if !exported {
            return Err("request handle already in use".into());
        }
        let result = self.pick(caller, mode, folder, close_rx).await;
        let _ = conn
            .object_server()
            .remove::<Request, _>(handle.as_ref())
            .await;
        result
    }

    /// `Ok(None)` means the user cancelled (or the app closed the request).
    async fn pick(
        &self,
        caller: Caller,
        mode: Mode,
        folder: Option<&Path>,
        close_rx: oneshot::Receiver<()>,
    ) -> Result<Option<Vec<String>>, String> {
        let config = Config::load()?;
        let request = PickRequest {
            start_dir: config::start_dir(folder, &config),
            mode,
            app_id: caller.app_id,
            title: caller.title,
        };
        let encoded = request.encode();
        if encoded.len() as u64 > MAX_FILE_SIZE {
            return Err("request too large".into());
        }
        let workspace = Workspace::create().map_err(|e| format!("workspace: {e}"))?;
        write_private(workspace.path(), REQUEST_FILE, &encoded)
            .map_err(|e| format!("workspace: {e}"))?;

        let argv = terminal::resolve(config.terminal.as_ref())?;
        let mut child = Command::new(&argv[0])
            .args(&argv[1..])
            .arg(&self.exe)
            .arg("pick")
            .arg(workspace.path())
            .current_dir(config::home_dir().unwrap_or_else(|| PathBuf::from("/")))
            .stdin(Stdio::null())
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("cannot start terminal {:?}: {e}", argv[0]))?;

        tokio::select! {
            status = child.wait() => {
                let status = status.map_err(|e| e.to_string())?;
                if !status.success() {
                    warn!(%status, "terminal exited with failure");
                }
            }
            _ = close_rx => {
                stop(&mut child).await;
                return Ok(None);
            }
        }

        let Some(bytes) =
            read_private(workspace.path(), RESULT_FILE).map_err(|e| format!("result: {e}"))?
        else {
            return Ok(None);
        };
        let paths = protocol::decode_paths(&bytes);
        check_result(&request.mode, &paths)?;
        paths
            .iter()
            .map(|p| file_uri(p).ok_or_else(|| "result path is not absolute".to_string()))
            .collect::<Result<Vec<_>, _>>()
            .map(Some)
    }
}

/// Asks the terminal's process group to quit, then forces it so a stuck
/// terminal cannot hold the request (and its workspace) forever.
async fn stop(child: &mut Child) {
    let Some(pid) = child.id() else { return };
    let group = -(pid as i32);
    // SAFETY: plain syscall; a negative pid targets the process group we created.
    unsafe { libc::kill(group, libc::SIGTERM) };
    if timeout(STOP_GRACE, child.wait()).await.is_err() {
        // SAFETY: as above.
        unsafe { libc::kill(group, libc::SIGKILL) };
        let _ = child.wait().await;
    }
}

/// What the app tells about itself; shown to the user, never trusted for decisions.
struct Caller {
    app_id: String,
    title: String,
}

impl Caller {
    fn new(app_id: String, title: String) -> Self {
        Self {
            app_id,
            title: title.chars().take(MAX_TITLE_CHARS).collect(),
        }
    }
}

/// Defence in depth: the picker is ours, but re-check what it returned.
fn check_result(mode: &Mode, paths: &[PathBuf]) -> Result<(), String> {
    if paths.is_empty() || paths.len() > MAX_RESULTS {
        return Err("result has no or too many paths".into());
    }
    let ok = match mode {
        Mode::Open {
            multiple,
            directory,
        } => {
            (*multiple || paths.len() == 1)
                && paths
                    .iter()
                    .all(|p| p.is_absolute() && p.exists() && p.is_dir() == *directory)
        }
        Mode::Save { .. } => paths.len() == 1 && is_save_target(&paths[0]),
        Mode::SaveMany { names } => {
            paths.len() == names.len()
                && paths
                    .iter()
                    .zip(names)
                    .all(|(p, n)| p.file_name() == Some(n.as_os_str()) && is_save_target(p))
        }
    };
    ok.then_some(())
        .ok_or_else(|| "result does not match the request".into())
}

fn is_save_target(path: &Path) -> bool {
    existing_parent(path).is_some() && !path.is_dir()
}

fn success(uris: Vec<String>) -> Response {
    let mut results = HashMap::new();
    let value = OwnedValue::try_from(Value::from(uris)).expect("string arrays carry no fds");
    results.insert("uris".to_string(), value);
    (RESPONSE_SUCCESS, results)
}

fn mode_name(mode: &Mode) -> &'static str {
    match mode {
        Mode::Open {
            directory: true, ..
        } => "open-directory",
        Mode::Open { .. } => "open",
        Mode::Save { .. } => "save",
        Mode::SaveMany { .. } => "save-many",
    }
}

/// Only xdg-desktop-portal may drive this backend: any other session-bus peer
/// could otherwise pop up pickers and learn what the user selects.
async fn ensure_frontend(
    conn: &Connection,
    header: &Header<'_>,
    handle: &OwnedObjectPath,
) -> fdo::Result<()> {
    let denied =
        || fdo::Error::AccessDenied("only xdg-desktop-portal may call this backend".into());
    let sender = header.sender().ok_or_else(denied)?;
    let dbus = fdo::DBusProxy::new(conn).await?;
    let frontend = BusName::try_from(FRONTEND_NAME).expect("valid bus name");
    let owner = dbus.get_name_owner(frontend).await.map_err(|_| denied())?;
    if owner.as_str() != sender.as_str() {
        warn!("rejected call from a peer that is not xdg-desktop-portal");
        return Err(denied());
    }
    if !handle.as_str().starts_with(REQUEST_PREFIX) {
        return Err(fdo::Error::InvalidArgs("unexpected request handle".into()));
    }
    Ok(())
}

pub struct Request {
    close: Mutex<Option<oneshot::Sender<()>>>,
}

#[interface(name = "org.freedesktop.impl.portal.Request")]
impl Request {
    async fn close(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<()> {
        let path = header
            .path()
            .map(|p| OwnedObjectPath::from(p.to_owned()))
            .ok_or_else(|| fdo::Error::Failed("no path".into()))?;
        ensure_frontend(conn, &header, &path).await?;
        if let Some(tx) = self.close.lock().await.take() {
            let _ = tx.send(());
        }
        Ok(())
    }
}

fn opt_bool(options: &Options, key: &str) -> bool {
    options
        .get(key)
        .and_then(|v| bool::try_from(v).ok())
        .unwrap_or(false)
}

fn opt_str<'a>(options: &'a Options, key: &str) -> Option<&'a str> {
    match &**options.get(key)? {
        Value::Str(s) => Some(s.as_str()),
        _ => None,
    }
}

fn opt_path(options: &Options, key: &str) -> Option<PathBuf> {
    bytes_of(options.get(key)?).and_then(|b| path_from_bytes(&b))
}

fn opt_byte_strings(options: &Options, key: &str) -> Option<Vec<Vec<u8>>> {
    match &**options.get(key)? {
        Value::Array(items) => items.iter().map(bytes_of).collect(),
        _ => None,
    }
}

fn bytes_of(value: &Value<'_>) -> Option<Vec<u8>> {
    match value {
        Value::Array(items) => items
            .iter()
            .map(|b| match b {
                Value::U8(b) => Some(*b),
                _ => None,
            })
            .collect(),
        _ => None,
    }
}

/// `SaveFiles` names must already be plain names; silently rewriting them could
/// make the app write somewhere other than the user approved.
fn name_from_bytes(bytes: &[u8]) -> Option<OsString> {
    let bytes = bytes.strip_suffix(&[0]).unwrap_or(bytes);
    let name = OsStr::from_bytes(bytes);
    crate::validate::is_plain_filename(name).then(|| name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_results_must_exist_and_match_kind() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        std::fs::write(&file, b"").unwrap();
        let open = |multiple, directory| Mode::Open {
            multiple,
            directory,
        };
        assert!(check_result(&open(false, false), std::slice::from_ref(&file)).is_ok());
        assert!(check_result(&open(false, false), &[file.clone(), file.clone()]).is_err());
        assert!(check_result(&open(false, false), &[dir.path().to_owned()]).is_err());
        assert!(check_result(&open(false, true), &[dir.path().to_owned()]).is_ok());
        assert!(check_result(&open(true, false), &[dir.path().join("missing")]).is_err());
        assert!(check_result(&open(true, false), &[]).is_err());
    }

    #[test]
    fn save_results_need_real_parent_and_matching_names() {
        let dir = tempfile::tempdir().unwrap();
        let save = Mode::Save { name: "x".into() };
        assert!(check_result(&save, &[dir.path().join("new")]).is_ok());
        assert!(check_result(&save, &[dir.path().to_owned()]).is_err());
        assert!(check_result(&save, &[dir.path().join("nope/new")]).is_err());
        let many = Mode::SaveMany {
            names: vec!["a".into(), "b".into()],
        };
        assert!(check_result(&many, &[dir.path().join("a"), dir.path().join("b")]).is_ok());
        assert!(check_result(&many, &[dir.path().join("a"), dir.path().join("c")]).is_err());
    }

    #[test]
    fn save_files_names_reject_paths() {
        assert_eq!(name_from_bytes(b"a.txt\0"), Some("a.txt".into()));
        assert_eq!(name_from_bytes(b"../a.txt\0"), None);
        assert_eq!(name_from_bytes(b"\0"), None);
    }
}
