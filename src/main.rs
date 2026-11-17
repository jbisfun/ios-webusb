mod devices;
mod state;
mod tray;
mod ui;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, bail};
use tokio::sync::mpsc;

use state::{Paths, State};

fn paths() -> anyhow::Result<Paths> {
    let data = dirs::data_dir()
        .context("no data directory")?
        .join("ios-webusb");
    let runtime = dirs::runtime_dir()
        .map(|dir| dir.join("ios-webusb"))
        .unwrap_or_else(|| data.clone());
    Ok(Paths {
        socket: std::env::var_os("IOS_WEBUSB_SOCKET")
            .map(PathBuf::from)
            .unwrap_or_else(|| runtime.join("usbmuxd")),
        lockdown: data.join("lockdown"),
    })
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,mdns_sd=warn".into()),
        )
        .init();

    let show = std::env::args().any(|arg| arg == "--show");
    let paths = paths()?;

    if std::os::unix::net::UnixStream::connect(&paths.socket).is_ok() {
        bail!(
            "another driver is already serving {}",
            paths.socket.display()
        );
    }
    if let Some(parent) = paths.socket.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if rusbmux::handler::set_lockdown_path(paths.lockdown.clone()).is_err() {
        bail!("lockdown path already set");
    }

    let runtime = tokio::runtime::Runtime::new()?;

    let (ui_tx, ui_rx) = mpsc::unbounded_channel();
    let (device_tx, device_rx) = mpsc::unbounded_channel();
    let state = State::new(paths, ui_tx, device_tx, runtime.handle().clone());

    runtime.spawn({
        let state = state.clone();
        let socket = state.paths.socket.to_string_lossy().into_owned();
        async move {
            if let Err(error) = rusbmux::daemon::run(Some(socket)).await {
                log::error!("USB driver stopped: {error}");
            }
            state.quit();
        }
    });
    runtime.block_on(wait_for_socket(&state));

    devices::spawn(state.clone(), device_rx);
    runtime.spawn(tray::run(state.clone()));
    log::info!("Serving usbmuxd on {}", state.paths.socket.display());

    ui::run(state.clone(), ui_rx, show);

    runtime.shutdown_timeout(Duration::from_millis(500));
    std::fs::remove_file(&state.paths.socket).ok();
    Ok(())
}

async fn wait_for_socket(state: &State) {
    for _ in 0..50 {
        if tokio::net::UnixStream::connect(&state.paths.socket)
            .await
            .is_ok()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    log::warn!(
        "usbmuxd socket {} did not come up",
        state.paths.socket.display()
    );
}
