use gpui::{
    App, AsyncApp, Bounds, Context, Div, ElementId, SharedString, Stateful, Task, TitlebarOptions,
    Window, WindowBounds, WindowHandle, WindowOptions, div, prelude::*, px, rgb, size,
};
use tokio::sync::mpsc;

use crate::state::{Snapshot, State, Transport, UiEvent};

pub fn run(state: State, mut events: mpsc::UnboundedReceiver<UiEvent>, show: bool) {
    gpui_platform::application().run(move |cx: &mut App| {
        let mut window: Option<WindowHandle<Panel>> = None;
        if show {
            window = open(&state, cx);
        }
        cx.spawn(async move |cx: &mut AsyncApp| {
            while let Some(event) = events.recv().await {
                match event {
                    UiEvent::ShowWindow => {
                        let activated = window.is_some_and(|handle| {
                            handle
                                .update(cx, |_, window, _| window.activate_window())
                                .is_ok()
                        });
                        if !activated {
                            window = cx.update(|cx| open(&state, cx));
                        }
                    }
                    UiEvent::Quit => {
                        cx.update(|cx| cx.quit());
                        return;
                    }
                }
            }
        })
        .detach();
    });
}

fn open(state: &State, cx: &mut App) -> Option<WindowHandle<Panel>> {
    let bounds = Bounds::centered(None, size(px(420.), px(520.)), cx);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: Some(TitlebarOptions {
            title: Some("iOS WebUSB".into()),
            ..Default::default()
        }),
        app_id: Some("ios-webusb".into()),
        ..Default::default()
    };
    let result = cx.open_window(options, |_, cx| cx.new(|cx| Panel::new(state.clone(), cx)));
    match result {
        Ok(handle) => {
            cx.activate(true);
            Some(handle)
        }
        Err(error) => {
            log::error!("Could not open the window: {error:#}");
            None
        }
    }
}

pub struct Panel {
    state: State,
    snapshot: Snapshot,
    _watch: Task<()>,
}

impl Panel {
    fn new(state: State, cx: &mut Context<Self>) -> Self {
        let mut revisions = state.subscribe();
        let watch = cx.spawn(async move |this, cx| {
            while revisions.changed().await.is_ok() {
                let updated = this.update(cx, |panel, cx| {
                    panel.snapshot = panel.state.snapshot();
                    cx.notify();
                });
                if updated.is_err() {
                    return;
                }
            }
        });
        Self {
            snapshot: state.snapshot(),
            state,
            _watch: watch,
        }
    }
}

const BACKGROUND: u32 = 0x1e1f22;
const SURFACE: u32 = 0x2b2d31;
const TEXT: u32 = 0xe6e6e6;
const MUTED: u32 = 0x9a9ca3;
const ACCENT: u32 = 0x3d6fd9;
const DANGER: u32 = 0xb5443c;

fn section(title: &'static str) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(div().text_xs().text_color(rgb(MUTED)).child(title))
}

fn row() -> Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .p_2()
        .rounded_md()
        .bg(rgb(SURFACE))
}

fn button(id: impl Into<ElementId>, label: &'static str, color: u32) -> Stateful<Div> {
    div()
        .id(id)
        .px_2()
        .py_1()
        .rounded_md()
        .bg(rgb(color))
        .text_sm()
        .cursor_pointer()
        .hover(|style| style.opacity(0.85))
        .child(label)
}

fn empty(text: &'static str) -> Div {
    div().text_sm().text_color(rgb(MUTED)).child(text)
}

impl Render for Panel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let snapshot = &self.snapshot;

        let mut requests = section("Requests");
        for (id, origin) in &snapshot.pending {
            let id = *id;
            requests =
                requests.child(
                    row()
                        .child(div().flex_1().text_sm().child(SharedString::from(format!(
                            "{origin} wants to connect to your devices"
                        ))))
                        .child(button(("allow", id), "Allow", ACCENT).on_click(
                            cx.listener(move |panel, _, _, _| panel.state.decide(id, true)),
                        ))
                        .child(button(("deny", id), "Deny", DANGER).on_click(
                            cx.listener(move |panel, _, _, _| panel.state.decide(id, false)),
                        )),
                );
        }

        let mut devices = section("Devices");
        if snapshot.devices.is_empty() {
            devices = devices.child(empty("Connect an iPhone or iPad over USB"));
        }
        for device in &snapshot.devices {
            let device_id = device.device_id;
            let transport = match device.transport {
                Transport::Usb => "USB",
                Transport::WiFi => "Wi-Fi",
            };
            let detail = match (&device.status, device.paired) {
                (Some(status), _) => status.clone(),
                (None, true) => format!("{transport} · paired · {}", device.udid),
                (None, false) => format!("{transport} · not paired · {}", device.udid),
            };
            devices = devices.child(
                row()
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .child(SharedString::from(device.name.clone()))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(MUTED))
                                    .child(SharedString::from(detail)),
                            ),
                    )
                    .child(
                        button(
                            ("pair", device_id as u64),
                            if device.paired { "Re-pair" } else { "Pair" },
                            ACCENT,
                        )
                        .on_click(cx.listener(move |panel, _, _, _| panel.state.pair(device_id))),
                    ),
            );
        }

        let mut sites = section("Allowed sites");
        if snapshot.allowed.is_empty() {
            sites = sites.child(empty("Sites you allow appear here"));
        }
        for (index, origin) in snapshot.allowed.iter().enumerate() {
            let connections = snapshot
                .sessions
                .iter()
                .filter(|session| &session.origin == origin)
                .count();
            let label = if connections == 0 {
                origin.clone()
            } else {
                format!("{origin} · {connections} open")
            };
            let origin = origin.clone();
            sites =
                sites.child(
                    row()
                        .child(div().flex_1().text_sm().child(SharedString::from(label)))
                        .child(button(("revoke", index), "Revoke", DANGER).on_click(
                            cx.listener(move |panel, _, _, _| panel.state.revoke(&origin)),
                        )),
                );
        }

        div()
            .id("panel")
            .size_full()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_4()
            .p_4()
            .bg(rgb(BACKGROUND))
            .text_color(rgb(TEXT))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(div().text_lg().child("iOS WebUSB"))
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child(SharedString::from(format!(
                                "Web apps: {} · usbmuxd: {}",
                                self.state.bridge_url(),
                                self.state.paths.socket.display()
                            ))),
                    ),
            )
            .when(!snapshot.pending.is_empty(), |panel| panel.child(requests))
            .child(devices)
            .child(sites)
    }
}
