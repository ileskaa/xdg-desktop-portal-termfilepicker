mod config;
mod pick;
mod portal;
mod protocol;
mod terminal;
mod validate;
mod workspace;

use std::ffi::OsString;
use std::io::IsTerminal;
use std::path::Path;
use std::process::ExitCode;

use tracing::{error, info, warn};

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    match args.as_slice() {
        [] => daemon(),
        [cmd, workspace] if cmd == "pick" => pick::main(Path::new(workspace)),
        [flag] if flag == "--version" => {
            println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("usage: {} [--version]", env!("CARGO_PKG_NAME"));
            ExitCode::from(2)
        }
    }
}

fn daemon() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .without_time()
        .with_ansi(std::io::stderr().is_terminal())
        .with_writer(std::io::stderr)
        .init();
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            error!("cannot start runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(serve()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("{e}");
            ExitCode::FAILURE
        }
    }
}

async fn serve() -> Result<(), Box<dyn std::error::Error>> {
    let exe = std::env::current_exe()?;
    let _conn = zbus::connection::Builder::session()?
        .serve_at(portal::OBJECT_PATH, portal::FileChooser::new(exe))?
        .name(portal::BUS_NAME)?
        .build()
        .await?;
    // Safe only after owning the name: no other instance can have live workspaces.
    if let Err(e) = workspace::remove_stale() {
        warn!("could not clean stale workspaces: {e}");
    }
    info!("ready");
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
    Ok(())
}
