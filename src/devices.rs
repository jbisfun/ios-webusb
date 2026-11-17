use std::collections::HashMap;
use std::time::Duration;

use futures_util::StreamExt;
use idevice::IdeviceService;
use idevice::services::lockdown::LockdownClient;
use idevice::usbmuxd::{Connection, UsbmuxdAddr, UsbmuxdDevice, UsbmuxdListenEvent};
use tokio::sync::mpsc;
use tokio::task::LocalSet;

use crate::state::{DeviceCommand, DeviceEntry, State, Transport};

const LABEL: &str = "ios-webusb";

/// Watches our own usbmuxd socket with idevice. It gets its own thread because
/// idevice's listen stream isn't `Send`.
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

enum Event {
    Connected(UsbmuxdDevice, String),
    Disconnected(u32),
}

async fn run(state: State, mut commands: mpsc::UnboundedReceiver<DeviceCommand>) {
    let address = UsbmuxdAddr::UnixSocket(state.paths.socket.to_string_lossy().into_owned());
    let (events_tx, mut events) = mpsc::unbounded_channel();
    tokio::task::spawn_local(watch(address.clone(), events_tx));

    let mut devices: HashMap<u32, UsbmuxdDevice> = HashMap::new();
    let mut tick = tokio::time::interval(Duration::from_secs(2));
    loop {
        tokio::select! {
            Some(event) = events.recv() => match event {
                Event::Connected(device, name) => {
                    state.upsert_device(entry(&state, &device, name));
                    devices.insert(device.device_id, device);
                }
                Event::Disconnected(device_id) => {
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
                    let address = address.clone();
                    tokio::task::spawn_local(async move {
                        let status = match pair(&address, &device).await {
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

/// Reconnects to the socket until the receiver is dropped; a lost connection
/// invalidates its device IDs, so they're reported as gone.
async fn watch(address: UsbmuxdAddr, events: mpsc::UnboundedSender<Event>) {
    let mut connected = Vec::new();
    while !events.is_closed() {
        if let Err(error) = listen(&address, &events, &mut connected).await {
            log::debug!("usbmuxd listen interrupted: {error}");
        }
        for device_id in connected.drain(..) {
            events.send(Event::Disconnected(device_id)).ok();
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn listen(
    address: &UsbmuxdAddr,
    events: &mpsc::UnboundedSender<Event>,
    connected: &mut Vec<u32>,
) -> Result<(), idevice::IdeviceError> {
    let mut connection = address.connect(0).await?;
    let mut stream = connection.listen().await?;
    while let Some(event) = stream.next().await {
        match event? {
            UsbmuxdListenEvent::Connected(device) => {
                let name = device_name(address, &device)
                    .await
                    .unwrap_or_else(|| device.udid.clone());
                connected.push(device.device_id);
                events.send(Event::Connected(device, name)).ok();
            }
            UsbmuxdListenEvent::Disconnected(device_id) => {
                connected.retain(|id| *id != device_id);
                events.send(Event::Disconnected(device_id)).ok();
            }
        }
    }
    Ok(())
}

async fn device_name(address: &UsbmuxdAddr, device: &UsbmuxdDevice) -> Option<String> {
    let lookup = async {
        let provider = device.to_provider(address.clone(), LABEL);
        let mut lockdown = LockdownClient::connect(&provider).await?;
        lockdown.get_value(Some("DeviceName"), None).await
    };
    match tokio::time::timeout(Duration::from_secs(3), lookup).await {
        Ok(Ok(value)) => value
            .as_string()
            .filter(|name| !name.is_empty())
            .map(str::to_owned),
        result => {
            log::warn!("Could not read the name of {}: {result:?}", device.udid);
            None
        }
    }
}

async fn pair(address: &UsbmuxdAddr, device: &UsbmuxdDevice) -> Result<(), idevice::IdeviceError> {
    let mut usbmuxd = address.connect(0).await?;
    let mut lockdown = LockdownClient::connect(&device.to_provider(address.clone(), LABEL)).await?;
    let host_id = uuid::Uuid::new_v4().to_string().to_uppercase();
    let buid = usbmuxd.get_buid().await?;
    let mut pairing_file = lockdown.pair(host_id, buid, None).await?;
    pairing_file.udid = Some(device.udid.clone());
    usbmuxd
        .save_pair_record(&device.udid, pairing_file.serialize()?)
        .await
}

fn entry(state: &State, device: &UsbmuxdDevice, name: String) -> DeviceEntry {
    DeviceEntry {
        device_id: device.device_id,
        udid: device.udid.clone(),
        name,
        transport: match device.connection_type {
            Connection::Network(_) => Transport::WiFi,
            _ => Transport::Usb,
        },
        paired: state.is_paired(&device.udid),
        status: None,
    }
}
