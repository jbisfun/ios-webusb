use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::{mpsc, oneshot, watch};

pub struct Paths {
    pub socket: PathBuf,
    pub lockdown: PathBuf,
    pub origins: PathBuf,
    pub bridge: SocketAddr,
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

#[derive(Clone, Debug)]
pub struct Session {
    pub id: u64,
    pub origin: String,
}

#[derive(Clone, Default)]
pub struct Snapshot {
    pub devices: Vec<DeviceEntry>,
    pub pending: Vec<(u64, String)>,
    pub allowed: Vec<String>,
    pub sessions: Vec<Session>,
}

pub enum UiEvent {
    ShowWindow,
    Quit,
}

pub enum DeviceCommand {
    Pair(u32),
}

struct PendingRequest {
    id: u64,
    origin: String,
    reply: oneshot::Sender<bool>,
}

struct LiveSession {
    session: Session,
    // Dropping this sender closes the bridge connection.
    _close: oneshot::Sender<()>,
}

#[derive(Default)]
struct Inner {
    devices: BTreeMap<u32, DeviceEntry>,
    pending: Vec<PendingRequest>,
    allowed: BTreeSet<String>,
    sessions: Vec<LiveSession>,
    next_id: u64,
}

impl Inner {
    fn next_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }
}

#[derive(Clone)]
pub struct State {
    inner: Arc<Mutex<Inner>>,
    revision: Arc<watch::Sender<u64>>,
    ui: mpsc::UnboundedSender<UiEvent>,
    devices: mpsc::UnboundedSender<DeviceCommand>,
    pub paths: Arc<Paths>,
}

/// Closes the session's bridge connection when the origin is revoked.
pub struct SessionHandle {
    state: State,
    id: u64,
    pub closed: oneshot::Receiver<()>,
}

impl Drop for SessionHandle {
    fn drop(&mut self) {
        let mut inner = self.state.lock();
        inner.sessions.retain(|live| live.session.id != self.id);
        drop(inner);
        self.state.changed();
    }
}

impl State {
    pub fn new(
        paths: Paths,
        ui: mpsc::UnboundedSender<UiEvent>,
        devices: mpsc::UnboundedSender<DeviceCommand>,
    ) -> Self {
        let allowed = std::fs::read(&paths.origins)
            .ok()
            .and_then(|data| serde_json::from_slice::<BTreeSet<String>>(&data).ok())
            .unwrap_or_default();
        let (revision, _) = watch::channel(0);
        Self {
            inner: Arc::new(Mutex::new(Inner {
                allowed,
                ..Inner::default()
            })),
            revision: Arc::new(revision),
            ui,
            devices,
            paths: Arc::new(paths),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
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
        let inner = self.lock();
        Snapshot {
            devices: inner.devices.values().cloned().collect(),
            pending: inner
                .pending
                .iter()
                .map(|request| (request.id, request.origin.clone()))
                .collect(),
            allowed: inner.allowed.iter().cloned().collect(),
            sessions: inner
                .sessions
                .iter()
                .map(|live| live.session.clone())
                .collect(),
        }
    }

    pub fn bridge_url(&self) -> String {
        format!("ws://{}", self.paths.bridge)
    }

    pub fn show_window(&self) {
        self.ui.send(UiEvent::ShowWindow).ok();
    }

    pub fn quit(&self) {
        self.ui.send(UiEvent::Quit).ok();
    }

    pub fn is_allowed(&self, origin: &str) -> bool {
        self.lock().allowed.contains(origin)
    }

    /// Queues a permission prompt, like WebUSB's `requestDevice` chooser.
    pub fn request_access(&self, origin: String) -> oneshot::Receiver<bool> {
        let (reply, receiver) = oneshot::channel();
        let mut inner = self.lock();
        let id = inner.next_id();
        inner.pending.push(PendingRequest { id, origin, reply });
        drop(inner);
        self.changed();
        self.show_window();
        receiver
    }

    /// Answers a prompt and every other one waiting on the same origin.
    pub fn decide(&self, id: u64, allow: bool) {
        let mut inner = self.lock();
        let Some(origin) = inner
            .pending
            .iter()
            .find(|request| request.id == id)
            .map(|request| request.origin.clone())
        else {
            return;
        };
        let (answered, waiting) = std::mem::take(&mut inner.pending)
            .into_iter()
            .partition::<Vec<_>, _>(|request| request.origin == origin);
        inner.pending = waiting;
        if allow {
            inner.allowed.insert(origin);
        }
        let allowed = allow.then(|| inner.allowed.clone());
        drop(inner);
        for request in answered {
            request.reply.send(allow).ok();
        }
        if let Some(allowed) = allowed {
            self.save_origins(&allowed);
        }
        self.changed();
    }

    /// Drops a pending prompt whose connection went away.
    pub fn cancel_request(&self, origin: &str) {
        let mut inner = self.lock();
        inner
            .pending
            .retain(|request| request.origin != origin || !request.reply.is_closed());
        drop(inner);
        self.changed();
    }

    pub fn revoke(&self, origin: &str) {
        let mut inner = self.lock();
        inner.allowed.remove(origin);
        inner.sessions.retain(|live| live.session.origin != origin);
        let allowed = inner.allowed.clone();
        drop(inner);
        self.save_origins(&allowed);
        self.changed();
    }

    fn save_origins(&self, allowed: &BTreeSet<String>) {
        let result = self
            .paths
            .origins
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| {
                let data = serde_json::to_vec_pretty(allowed).map_err(std::io::Error::other)?;
                std::fs::write(&self.paths.origins, data)
            });
        if let Err(error) = result {
            log::error!("Could not save {}: {error}", self.paths.origins.display());
        }
    }

    pub fn open_session(&self, origin: String) -> SessionHandle {
        let (close, closed) = oneshot::channel();
        let mut inner = self.lock();
        let id = inner.next_id();
        inner.sessions.push(LiveSession {
            session: Session { id, origin },
            _close: close,
        });
        drop(inner);
        self.changed();
        SessionHandle {
            state: self.clone(),
            id,
            closed,
        }
    }

    pub fn pair(&self, device_id: u32) {
        self.devices.send(DeviceCommand::Pair(device_id)).ok();
    }

    pub fn is_paired(&self, udid: &str) -> bool {
        self.paths.lockdown.join(format!("{udid}.plist")).exists()
    }

    pub fn upsert_device(&self, device: DeviceEntry) {
        self.lock().devices.insert(device.device_id, device);
        self.changed();
    }

    pub fn remove_device(&self, device_id: u32) {
        self.lock().devices.remove(&device_id);
        self.changed();
    }

    pub fn set_device_status(&self, device_id: u32, status: Option<String>) {
        if let Some(device) = self.lock().devices.get_mut(&device_id) {
            device.status = status;
        }
        self.changed();
    }

    /// Picks up pair records written by the driver's own preflight.
    pub fn refresh_pairing(&self) {
        let mut inner = self.lock();
        let mut dirty = false;
        for device in inner.devices.values_mut() {
            let paired = self.is_paired(&device.udid);
            if device.paired != paired {
                device.paired = paired;
                dirty = true;
            }
        }
        drop(inner);
        if dirty {
            self.changed();
        }
    }
}
