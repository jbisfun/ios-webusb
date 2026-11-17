use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::{mpsc, watch};

pub struct Paths {
    pub socket: PathBuf,
    pub lockdown: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    Usb,
    WiFi,
}

#[derive(Clone, Debug)]
pub struct DeviceEntry {
    pub device_id: u32,
    pub udid: String,
    pub name: String,
    pub transport: Transport,
    pub paired: bool,
    pub status: Option<String>,
}

#[derive(Clone, Default)]
pub struct Snapshot {
    pub devices: Vec<DeviceEntry>,
}

pub enum UiEvent {
    ShowWindow,
    Quit,
}

pub enum DeviceCommand {
    Pair(u32),
}

#[derive(Clone)]
pub struct State {
    devices: Arc<Mutex<BTreeMap<u32, DeviceEntry>>>,
    revision: Arc<watch::Sender<u64>>,
    ui: mpsc::UnboundedSender<UiEvent>,
    commands: mpsc::UnboundedSender<DeviceCommand>,
    runtime: tokio::runtime::Handle,
    pub paths: Arc<Paths>,
}

impl State {
    pub fn new(
        paths: Paths,
        ui: mpsc::UnboundedSender<UiEvent>,
        commands: mpsc::UnboundedSender<DeviceCommand>,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        let (revision, _) = watch::channel(0);
        Self {
            devices: Arc::default(),
            revision: Arc::new(revision),
            ui,
            commands,
            runtime,
            paths: Arc::new(paths),
        }
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<u32, DeviceEntry>> {
        self.devices
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn changed(&self) {
        self.revision.send_modify(|revision| *revision += 1);
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.revision.subscribe()
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            devices: self.lock().values().cloned().collect(),
        }
    }

    pub fn show_window(&self) {
        self.ui.send(UiEvent::ShowWindow).ok();
    }

    pub fn quit(&self) {
        self.ui.send(UiEvent::Quit).ok();
    }

    pub fn pair(&self, device_id: u32) {
        self.commands.send(DeviceCommand::Pair(device_id)).ok();
    }

    /// Lets go of the device's USB interface so a browser can claim it over WebUSB.
    /// The driver takes it back the next time it's plugged in.
    pub fn release(&self, device_id: u32) {
        let state = self.clone();
        self.runtime.spawn(async move {
            match rusbmux::watcher::remove_device(u64::from(device_id)).await {
                Ok(_) => state.remove_device(device_id),
                Err(error) => {
                    state.set_device_status(device_id, Some(format!("Release failed: {error}")))
                }
            }
        });
    }

    pub fn is_paired(&self, udid: &str) -> bool {
        self.paths.lockdown.join(format!("{udid}.plist")).exists()
    }

    pub fn upsert_device(&self, device: DeviceEntry) {
        self.lock().insert(device.device_id, device);
        self.changed();
    }

    pub fn remove_device(&self, device_id: u32) {
        self.lock().remove(&device_id);
        self.changed();
    }

    pub fn set_device_status(&self, device_id: u32, status: Option<String>) {
        if let Some(device) = self.lock().get_mut(&device_id) {
            device.status = status;
        }
        self.changed();
    }

    /// Picks up pair records written by the driver's own preflight.
    pub fn refresh_pairing(&self) {
        let mut devices = self.lock();
        let mut dirty = false;
        for device in devices.values_mut() {
            let paired = self.is_paired(&device.udid);
            if device.paired != paired {
                device.paired = paired;
                dirty = true;
            }
        }
        drop(devices);
        if dirty {
            self.changed();
        }
    }
}
