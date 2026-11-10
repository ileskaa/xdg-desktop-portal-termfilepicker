//! End-to-end: private D-Bus bus, the real daemon binary, a fake terminal and a
//! fake Yazi. Verifies the portal contract and the security properties
//! described in the README.

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use zbus::zvariant::{OwnedValue, Value};
use zbus::{Connection, connection};

const BUS_NAME: &str = "org.freedesktop.impl.portal.desktop.termfilepicker";
const PATH: &str = "/org/freedesktop/portal/desktop";
const IFACE: &str = "org.freedesktop.impl.portal.FileChooser";
const HANDLE: &str = "/org/freedesktop/portal/desktop/request/1_1/t";
/// A folder name that must never leak into argv or logs.
const SECRET: &str = "secret-folder-7f3a";

const FAKE_TERMINAL: &str = r#"#!/bin/sh
[ -f "$ROOT/ignore-term" ] && trap '' TERM
printf '%s\n' "$@" >> "$ROOT/terminal-argv"
input=/dev/null
[ -f "$ROOT/input" ] && input="$ROOT/input"
exec "$@" < "$input" > "$ROOT/pick-output" 2>&1
"#;

const FAKE_YAZI: &str = r#"#!/bin/sh
printf '%s\n' "$@" >> "$ROOT/yazi-argv"
printf '%s' "$PWD" > "$ROOT/yazi-pwd"
for a in "$@"; do
  case "$a" in
    --chooser-file=*) chooser="${a#*=}" ;;
    --cwd-file=*) cwdfile="${a#*=}" ;;
  esac
done
[ -f "$ROOT/hang" ] && sleep 30
[ -f "$ROOT/chosen" ] && cp "$ROOT/chosen" "$chooser"
[ -f "$ROOT/quit-here" ] && printf '%s' "$PWD" > "$cwdfile"
exit 0
"#;

struct Env {
    root: tempfile::TempDir,
    bus: Child,
    daemon: Child,
    address: String,
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.bus.kill();
        let _ = self.daemon.wait();
        let _ = self.bus.wait();
    }
}

impl Env {
    fn start() -> Self {
        let root = tempfile::tempdir().unwrap();
        let r = root.path();
        for dir in [
            "runtime",
            "config/xdg-desktop-portal-termfilepicker",
            "bin",
            "home",
        ] {
            fs::create_dir_all(r.join(dir)).unwrap();
        }
        fs::set_permissions(r.join("runtime"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::create_dir_all(r.join(SECRET)).unwrap();
        fs::write(r.join(SECRET).join("a.txt"), b"hello").unwrap();
        script(&r.join("bin/fake-terminal"), FAKE_TERMINAL);
        script(&r.join("bin/fake-yazi"), FAKE_YAZI);
        let config = format!(
            "terminal = [{:?}]\nyazi = {:?}\n",
            r.join("bin/fake-terminal"),
            r.join("bin/fake-yazi")
        );
        fs::write(
            r.join("config/xdg-desktop-portal-termfilepicker/config.toml"),
            config,
        )
        .unwrap();

        let mut bus = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .arg(format!("--address=unix:path={}", r.join("bus").display()))
            .stdout(Stdio::piped())
            .spawn()
            .expect("dbus-daemon is required for integration tests");
        let address = read_line(bus.stdout.take().unwrap());

        let daemon = Command::new(env!("CARGO_BIN_EXE_xdg-desktop-portal-termfilepicker"))
            .env("DBUS_SESSION_BUS_ADDRESS", &address)
            .env("XDG_RUNTIME_DIR", r.join("runtime"))
            .env("XDG_CONFIG_HOME", r.join("config"))
            .env("HOME", r.join("home"))
            .env("ROOT", r)
            .env("RUST_LOG", "trace")
            .stdout(fs::File::create(r.join("daemon-stdout")).unwrap())
            .stderr(fs::File::create(r.join("daemon-log")).unwrap())
            .spawn()
            .unwrap();
        Self {
            root,
            bus,
            daemon,
            address,
        }
    }

    fn root(&self) -> &Path {
        self.root.path()
    }

    fn secret_dir(&self) -> PathBuf {
        self.root().join(SECRET)
    }

    fn set(&self, name: &str, contents: &str) {
        fs::write(self.root().join(name), contents).unwrap();
    }

    fn read(&self, name: &str) -> String {
        fs::read_to_string(self.root().join(name)).unwrap_or_default()
    }

    /// Connects; `as_frontend` claims xdg-desktop-portal's well-known name.
    async fn client(&self, as_frontend: bool) -> Connection {
        let mut builder = connection::Builder::address(self.address.as_str()).unwrap();
        if as_frontend {
            builder = builder.name("org.freedesktop.portal.Desktop").unwrap();
        }
        let conn = builder.build().await.unwrap();
        wait_for_backend(&conn).await;
        conn
    }

    fn assert_no_leaks(&self) {
        for file in ["terminal-argv", "yazi-argv", "daemon-log"] {
            let text = self.read(file);
            assert!(!text.contains(SECRET), "{file} leaks the folder:\n{text}");
            assert!(!text.contains("a.txt"), "{file} leaks a file name:\n{text}");
            assert!(
                !text.contains("report"),
                "{file} leaks the save name:\n{text}"
            );
        }
        let base = self
            .root()
            .join("runtime/xdg-desktop-portal-termfilepicker");
        let leftovers: Vec<_> = fs::read_dir(base).unwrap().collect();
        assert!(leftovers.is_empty(), "workspace not removed: {leftovers:?}");
    }
}

fn script(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn read_line(stdout: std::process::ChildStdout) -> String {
    use std::io::BufRead;
    let mut line = String::new();
    std::io::BufReader::new(stdout)
        .read_line(&mut line)
        .unwrap();
    line.trim().to_string()
}

async fn wait_for_backend(conn: &Connection) {
    let dbus = zbus::fdo::DBusProxy::new(conn).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !dbus
        .name_has_owner(BUS_NAME.try_into().unwrap())
        .await
        .unwrap()
    {
        assert!(Instant::now() < deadline, "backend did not start");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn folder_option(dir: &Path) -> Value<'static> {
    let mut bytes = dir.as_os_str().as_encoded_bytes().to_vec();
    bytes.push(0);
    Value::from(bytes)
}

type Reply = (u32, HashMap<String, OwnedValue>);

async fn call(
    conn: &Connection,
    method: &str,
    options: HashMap<&str, Value<'_>>,
) -> zbus::Result<Reply> {
    call_at(conn, HANDLE, method, options).await
}

async fn call_at(
    conn: &Connection,
    handle: &str,
    method: &str,
    options: HashMap<&str, Value<'_>>,
) -> zbus::Result<Reply> {
    let msg = conn
        .call_method(
            Some(BUS_NAME),
            PATH,
            Some(IFACE),
            method,
            &(
                zbus::zvariant::ObjectPath::try_from(handle).unwrap(),
                "org.test.App",
                "",
                "Pick",
                options,
            ),
        )
        .await?;
    msg.body().deserialize::<Reply>()
}

fn uris(reply: &Reply) -> Vec<String> {
    let value = reply.1.get("uris").expect("uris in results");
    Vec::<String>::try_from(value.try_clone().unwrap()).unwrap()
}

fn uri(path: &Path) -> String {
    format!("file://{}", path.display())
}

#[tokio::test]
async fn open_returns_selection_without_leaking_paths() {
    let env = Env::start();
    let file = env.secret_dir().join("a.txt");
    env.set("chosen", &format!("{}\n", file.display()));
    let conn = env.client(true).await;

    let opts = HashMap::from([("current_folder", folder_option(&env.secret_dir()))]);
    let reply = call(&conn, "OpenFile", opts).await.unwrap();

    assert_eq!(reply.0, 0);
    assert_eq!(uris(&reply), [uri(&file)]);
    assert_eq!(
        env.read("yazi-pwd"),
        env.secret_dir().display().to_string(),
        "start dir via cwd"
    );
    env.assert_no_leaks();
}

#[tokio::test]
async fn only_xdg_desktop_portal_may_call() {
    let env = Env::start();
    env.set(
        "chosen",
        &format!("{}\n", env.secret_dir().join("a.txt").display()),
    );
    let _frontend = env.client(true).await;
    let intruder = env.client(false).await;

    let err = call(&intruder, "OpenFile", HashMap::new())
        .await
        .unwrap_err();

    assert!(err.to_string().contains("AccessDenied"), "{err}");
    assert_eq!(
        env.read("terminal-argv"),
        "",
        "no terminal for a rejected caller"
    );
}

#[tokio::test]
async fn open_single_rejects_multiple_selection() {
    let env = Env::start();
    let file = env.secret_dir().join("a.txt");
    env.set("chosen", &format!("{0}\n{0}\n", file.display()));
    let conn = env.client(true).await;

    let reply = call(&conn, "OpenFile", HashMap::new()).await.unwrap();

    assert_eq!(reply.0, 1, "picker refuses, then input closes → cancelled");
}

#[tokio::test]
async fn save_new_name_creates_nothing_on_disk() {
    let env = Env::start();
    env.set("quit-here", "");
    env.set("input", "\n");
    let conn = env.client(true).await;

    let opts = HashMap::from([
        ("current_folder", folder_option(&env.secret_dir())),
        ("current_name", Value::from("../report.pdf")),
    ]);
    let reply = call(&conn, "SaveFile", opts).await.unwrap();

    let target = env.secret_dir().join("report.pdf");
    assert_eq!(reply.0, 0, "{}", env.read("pick-output"));
    assert_eq!(
        uris(&reply),
        [uri(&target)],
        "suggested name reduced to a basename"
    );
    assert!(!target.exists(), "portal must not create placeholder files");
    let entries = fs::read_dir(env.secret_dir()).unwrap().count();
    assert_eq!(entries, 1, "target folder untouched");
    env.assert_no_leaks();
}

#[tokio::test]
async fn save_over_existing_file_needs_confirmation() {
    let env = Env::start();
    let existing = env.secret_dir().join("a.txt");
    env.set("chosen", &format!("{}\n", existing.display()));
    let conn = env.client(true).await;

    env.set("input", "n\n");
    let declined = call(&conn, "SaveFile", HashMap::new()).await.unwrap();
    env.set("input", "y\n");
    let accepted = call(&conn, "SaveFile", HashMap::new()).await.unwrap();

    assert_eq!(declined.0, 1);
    assert_eq!(accepted.0, 0);
    assert_eq!(uris(&accepted), [uri(&existing)]);
    assert_eq!(fs::read(&existing).unwrap(), b"hello", "content untouched");
}

#[tokio::test]
async fn save_files_rejects_path_names() {
    let env = Env::start();
    let conn = env.client(true).await;

    let opts = HashMap::from([("files", Value::from(vec![b"../evil\0".to_vec()]))]);
    let err = call(&conn, "SaveFiles", opts).await.unwrap_err();

    assert!(err.to_string().contains("InvalidArgs"), "{err}");
}

fn spawn_open(conn: &Connection, handle: &'static str) -> tokio::task::JoinHandle<Reply> {
    let conn = conn.clone();
    tokio::spawn(async move {
        call_at(&conn, handle, "OpenFile", HashMap::new())
            .await
            .unwrap()
    })
}

async fn wait_for_pickers(env: &Env, count: usize) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while env.read("yazi-argv").lines().count() < count * 2 {
        assert!(Instant::now() < deadline, "picker never started");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn close(conn: &Connection, handle: &str) -> zbus::Result<()> {
    conn.call_method(
        Some(BUS_NAME),
        handle,
        Some("org.freedesktop.impl.portal.Request"),
        "Close",
        &(),
    )
    .await
    .map(|_| ())
}

async fn reply_within(secs: u64, pending: tokio::task::JoinHandle<Reply>) -> Reply {
    tokio::time::timeout(Duration::from_secs(secs), pending)
        .await
        .expect("request did not finish in time")
        .unwrap()
}

#[tokio::test]
async fn close_cancels_and_cleans_up() {
    let env = Env::start();
    env.set("hang", "");
    let conn = env.client(true).await;

    let pending = spawn_open(&conn, HANDLE);
    wait_for_pickers(&env, 1).await;
    close(&conn, HANDLE).await.unwrap();

    assert_eq!(reply_within(5, pending).await.0, 1);
    env.assert_no_leaks();
}

#[tokio::test]
async fn close_kills_a_terminal_that_ignores_sigterm() {
    let env = Env::start();
    env.set("hang", "");
    env.set("ignore-term", "");
    let conn = env.client(true).await;

    let pending = spawn_open(&conn, HANDLE);
    wait_for_pickers(&env, 1).await;
    close(&conn, HANDLE).await.unwrap();

    assert_eq!(reply_within(10, pending).await.0, 1);
    env.assert_no_leaks();
}

#[tokio::test]
async fn reused_handle_does_not_hijack_the_running_request() {
    let env = Env::start();
    env.set("hang", "");
    let conn = env.client(true).await;

    let first = spawn_open(&conn, HANDLE);
    wait_for_pickers(&env, 1).await;
    let second = call(&conn, "OpenFile", HashMap::new()).await.unwrap();
    assert_eq!(second.0, 2, "duplicate handle must fail");

    close(&conn, HANDLE)
        .await
        .expect("first request must still be closable");
    assert_eq!(reply_within(5, first).await.0, 1);
}

#[tokio::test]
async fn concurrent_pickers_are_capped() {
    const HANDLES: [&str; 4] = [
        "/org/freedesktop/portal/desktop/request/1_1/a",
        "/org/freedesktop/portal/desktop/request/1_1/b",
        "/org/freedesktop/portal/desktop/request/1_1/c",
        "/org/freedesktop/portal/desktop/request/1_1/d",
    ];
    let env = Env::start();
    env.set("hang", "");
    let conn = env.client(true).await;

    let running: Vec<_> = HANDLES.iter().map(|h| spawn_open(&conn, h)).collect();
    wait_for_pickers(&env, HANDLES.len()).await;
    let extra = call(&conn, "OpenFile", HashMap::new()).await.unwrap();
    assert_eq!(extra.0, 2, "fifth concurrent picker must be refused");

    for (handle, pending) in HANDLES.iter().zip(running) {
        close(&conn, handle).await.unwrap();
        assert_eq!(reply_within(5, pending).await.0, 1);
    }
}

#[tokio::test]
async fn save_suggestion_with_escape_sequences_is_not_shown() {
    let env = Env::start();
    env.set("quit-here", "");
    env.set("input", "\n");
    let conn = env.client(true).await;

    let opts = HashMap::from([
        ("current_folder", folder_option(&env.secret_dir())),
        (
            "current_name",
            Value::from("\x1b]0;pwned\x07\x1b[2Jevil.txt"),
        ),
    ]);
    let reply = call(&conn, "SaveFile", opts).await.unwrap();

    let output = env.read("pick-output");
    assert!(
        !output.contains('\x1b'),
        "escape reached the terminal: {output:?}"
    );
    assert_eq!(reply.0, 0, "{output}");
    assert_eq!(uris(&reply), [uri(&env.secret_dir().join("untitled"))]);
}

#[tokio::test]
async fn save_files_rejects_names_with_control_characters() {
    let env = Env::start();
    let conn = env.client(true).await;

    let opts = HashMap::from([("files", Value::from(vec![b"a\x1b[2J.txt\0".to_vec()]))]);
    let err = call(&conn, "SaveFiles", opts).await.unwrap_err();

    assert!(err.to_string().contains("InvalidArgs"), "{err}");
}

#[tokio::test]
async fn prompts_name_the_requesting_app() {
    let env = Env::start();
    env.set("quit-here", "");
    env.set("input", "\n");
    let conn = env.client(true).await;

    let reply = call(&conn, "SaveFile", HashMap::new()).await.unwrap();

    assert_eq!(reply.0, 0);
    assert!(
        env.read("pick-output").contains("org.test.App asks: Pick"),
        "{}",
        env.read("pick-output")
    );
}

#[tokio::test]
async fn refuses_to_work_without_a_private_runtime_dir() {
    let env = Env::start();
    env.set(
        "chosen",
        &format!("{}\n", env.secret_dir().join("a.txt").display()),
    );
    let conn = env.client(true).await;
    fs::set_permissions(
        env.root().join("runtime"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();

    let reply = call(&conn, "OpenFile", HashMap::new()).await.unwrap();

    assert_eq!(reply.0, 2, "must fail instead of falling back to /tmp");
    assert_eq!(env.read("terminal-argv"), "");
}
