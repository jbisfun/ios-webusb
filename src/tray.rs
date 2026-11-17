use ksni::menu::{MenuItem, StandardItem, SubMenu};
use ksni::{Status, TrayMethods};

use crate::state::{Snapshot, State, Transport};

struct Tray {
    state: State,
    snapshot: Snapshot,
}

pub async fn run(state: State) {
    let tray = Tray {
        snapshot: state.snapshot(),
        state: state.clone(),
    };
    let handle = match tray.spawn().await {
        Ok(handle) => handle,
        Err(error) => {
            log::error!("Could not create the tray icon: {error}");
            return;
        }
    };
    let mut revisions = state.subscribe();
    while revisions.changed().await.is_ok() {
        let snapshot = state.snapshot();
        if handle
            .update(|tray| tray.snapshot = snapshot)
            .await
            .is_none()
        {
            return;
        }
    }
}

fn item(label: impl Into<String>, activate: impl Fn(&mut Tray) + Send + 'static) -> MenuItem<Tray> {
    StandardItem {
        label: label.into(),
        activate: Box::new(activate),
        ..Default::default()
    }
    .into()
}

fn note(label: impl Into<String>) -> MenuItem<Tray> {
    StandardItem {
        label: label.into(),
        enabled: false,
        ..Default::default()
    }
    .into()
}

impl ksni::Tray for Tray {
    fn id(&self) -> String {
        "ios-webusb".into()
    }

    fn title(&self) -> String {
        "iOS WebUSB".into()
    }

    fn icon_name(&self) -> String {
        "phone-symbolic".into()
    }

    fn attention_icon_name(&self) -> String {
        "dialog-question-symbolic".into()
    }

    fn status(&self) -> Status {
        if self.snapshot.pending.is_empty() {
            Status::Active
        } else {
            Status::NeedsAttention
        }
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        let devices = self.snapshot.devices.len();
        ksni::ToolTip {
            title: "iOS WebUSB".into(),
            description: format!(
                "{devices} device(s), {} web connection(s) on {}",
                self.snapshot.sessions.len(),
                self.state.bridge_url()
            ),
            ..Default::default()
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.state.show_window();
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut menu = Vec::new();

        for (id, origin) in &self.snapshot.pending {
            let id = *id;
            menu.push(
                SubMenu {
                    label: format!("{origin} wants device access"),
                    submenu: vec![
                        item("Allow", move |tray: &mut Tray| tray.state.decide(id, true)),
                        item("Deny", move |tray: &mut Tray| tray.state.decide(id, false)),
                    ],
                    ..Default::default()
                }
                .into(),
            );
        }
        if !self.snapshot.pending.is_empty() {
            menu.push(MenuItem::Separator);
        }

        if self.snapshot.devices.is_empty() {
            menu.push(note("No devices"));
        }
        for device in &self.snapshot.devices {
            let transport = match device.transport {
                Transport::Usb => "USB",
                Transport::WiFi => "Wi-Fi",
            };
            let device_id = device.device_id;
            let mut submenu = vec![note(device.udid.clone())];
            if let Some(status) = &device.status {
                submenu.push(note(status.clone()));
            }
            submenu.push(item(
                if device.paired { "Pair again" } else { "Pair" },
                move |tray: &mut Tray| tray.state.pair(device_id),
            ));
            menu.push(
                SubMenu {
                    label: format!(
                        "{} ({transport}{})",
                        device.name,
                        if device.paired { "" } else { ", not paired" }
                    ),
                    submenu,
                    ..Default::default()
                }
                .into(),
            );
        }

        if !self.snapshot.allowed.is_empty() {
            menu.push(MenuItem::Separator);
            menu.push(
                SubMenu {
                    label: "Allowed sites".into(),
                    submenu: self
                        .snapshot
                        .allowed
                        .iter()
                        .map(|origin| {
                            let origin = origin.clone();
                            item(format!("Revoke {origin}"), move |tray: &mut Tray| {
                                tray.state.revoke(&origin)
                            })
                        })
                        .collect(),
                    ..Default::default()
                }
                .into(),
            );
        }

        menu.push(MenuItem::Separator);
        menu.push(item("Open", |tray: &mut Tray| tray.state.show_window()));
        menu.push(item("Quit", |tray: &mut Tray| tray.state.quit()));
        menu
    }
}
