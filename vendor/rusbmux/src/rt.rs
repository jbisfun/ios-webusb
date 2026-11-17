//! Task spawning and timers for native (tokio) and WebUSB (browser event loop) builds.

use std::{future::Future, time::Duration};

#[cfg(not(target_arch = "wasm32"))]
pub(crate) type Task = tokio::task::JoinHandle<()>;

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn spawn(future: impl Future<Output = ()> + Send + 'static) -> Task {
    tokio::spawn(future)
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) async fn sleep(duration: Duration) {
    tokio::time::sleep(duration).await;
}

#[cfg(target_arch = "wasm32")]
#[derive(Debug)]
pub(crate) struct Task(tokio_util::sync::CancellationToken);

#[cfg(target_arch = "wasm32")]
impl Task {
    pub(crate) fn abort(&self) {
        self.0.cancel();
    }
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn spawn(future: impl Future<Output = ()> + 'static) -> Task {
    let token = tokio_util::sync::CancellationToken::new();
    let cancelled = token.clone();
    wasm_bindgen_futures::spawn_local(async move {
        tokio::select! {
            () = future => {}
            () = cancelled.cancelled() => {}
        }
    });
    Task(token)
}

#[cfg(target_arch = "wasm32")]
pub(crate) async fn sleep(duration: Duration) {
    wasmtimer::tokio::sleep(duration).await;
}
