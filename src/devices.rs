use std::collections::HashMap;
use std::time::Duration;

use idevice::usbmuxd::{Connection, UsbmuxdAddr};
use plume_utils::{Device, DeviceEvent};
use tokio::sync::mpsc;
use tokio::task::LocalSet;

use crate::state::{DeviceCommand, DeviceEntry, State, Transport};

/// Runs Jimpactor's device discovery against our own usbmuxd socket. It gets its own
/// thread because plume_utils' listen stream isn't `Send`.
pub fn spawn(state: State, commands: mpsc::UnboundedReceiver<DeviceCommand>) {
    let spawned = std::thread::Builder::new()
        .name("devices".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    log::error!("Could not start the device runtime: {error}");
                    return;
                }
            };
            LocalSet::new().block_on(&runtime, run(state, commands));
        });
    if let Err(error) = spawned {
        log::error!("Could not spawn the device thread: {error}");
    }
}

async fn run(state: State, mut commands: mpsc::UnboundedReceiver<DeviceCommand>) {
    let address = UsbmuxdAddr::UnixSocket(state.paths.socket.to_string_lossy().into_owned());
    let (events_tx, mut events) = mpsc::unbounded_channel();
    tokio::task::spawn_local(plume_utils::watch_devices(address, events_tx));

    let mut devices: HashMap<u32, Device> = HashMap::new();
    let mut tick = tokio::time::interval(Duration::from_secs(2));
    loop {
        tokio::select! {
            Some(event) = events.recv() => match event {
                DeviceEvent::Connected(device) => {
                    state.upsert_device(entry(&state, &device));
                    devices.insert(device.device_id, device);
                }
                DeviceEvent::Disconnected(device_id) => {
                    devices.remove(&device_id);
                    state.remove_device(device_id);
                }
            },
            Some(command) = commands.recv() => match command {
                DeviceCommand::Pair(device_id) => {
                    let Some(device) = devices.get(&device_id).cloned() else { continue };
                    state.set_device_status(
                        device_id,
                        Some("Pairing: unlock the device and tap Trust".into()),
                    );
                    let state = state.clone();
                    tokio::task::spawn_local(async move {
                        let status = match device.pair().await {
                            Ok(()) => None,
                            Err(error) => Some(format!("Pairing failed: {error}")),
                        };
                        state.set_device_status(device_id, status);
                        state.refresh_pairing();
                    });
                }
            },
            _ = tick.tick() => state.refresh_pairing(),
            else => return,
        }
    }
}

fn entry(state: &State, device: &Device) -> DeviceEntry {
    let transport = match device
        .usbmuxd_device
        .as_ref()
        .map(|usb| &usb.connection_type)
    {
        Some(Connection::Network(_)) => Transport::WiFi,
        _ if device.network.is_some() => Transport::WiFi,
        _ => Transport::Usb,
    };
    DeviceEntry {
        device_id: device.device_id,
        udid: device.udid.clone(),
        name: device.name.clone(),
        transport,
        paired: state.is_paired(&device.udid),
        status: None,
    }
}
