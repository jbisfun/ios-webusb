//! WebSocket bridge: every connection is one usbmuxd client connection, so web apps
//! speak the usbmuxd protocol (ListDevices, Listen, Connect, ReadPairRecord, ...) in
//! binary frames. Browser origins need the user's approval, like WebUSB's chooser.

use std::time::Duration;

use anyhow::{Context, bail};
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UnixStream};
use tokio_tungstenite::tungstenite::Message;

use crate::state::State;

const MAX_HEADER: usize = 16 * 1024;
const PROMPT_TIMEOUT: Duration = Duration::from_secs(120);

pub async fn serve(state: State, listener: TcpListener) {
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                let state = state.clone();
                tokio::spawn(async move {
                    if let Err(error) = handle(state, stream).await {
                        log::debug!("Bridge connection from {peer} ended: {error:#}");
                    }
                });
            }
            Err(error) => {
                log::warn!("Bridge accept failed: {error}");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
}

async fn handle(state: State, mut stream: TcpStream) -> anyhow::Result<()> {
    let origin = tokio::time::timeout(Duration::from_secs(10), peek_origin(&stream))
        .await
        .context("timed out reading the handshake")??;

    // Local programs send no Origin; they could use the usbmuxd socket directly anyway.
    let label = match origin {
        Some(origin) => {
            if !state.is_allowed(&origin) && !ask(&state, &origin).await {
                stream
                    .write_all(
                        b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await
                    .ok();
                bail!("{origin} was denied");
            }
            origin
        }
        None => "local client".to_string(),
    };

    let websocket = tokio_tungstenite::accept_async(stream).await?;
    let mux = UnixStream::connect(&state.paths.socket)
        .await
        .context("connecting to the usbmuxd socket")?;
    let mut session = state.open_session(label);
    pump(websocket, mux, &mut session.closed).await
}

async fn ask(state: &State, origin: &str) -> bool {
    let decision = state.request_access(origin.to_string());
    let allowed = matches!(
        tokio::time::timeout(PROMPT_TIMEOUT, decision).await,
        Ok(Ok(true))
    );
    if !allowed {
        state.cancel_request(origin);
    }
    allowed
}

/// Reads the Origin header without consuming the request, so tungstenite can still
/// perform the handshake afterwards.
async fn peek_origin(stream: &TcpStream) -> anyhow::Result<Option<String>> {
    let mut buffer = vec![0u8; MAX_HEADER];
    let mut seen = 0;
    loop {
        let length = stream.peek(&mut buffer).await?;
        if length == 0 {
            bail!("connection closed during handshake");
        }
        let mut headers = [httparse::EMPTY_HEADER; 64];
        let mut request = httparse::Request::new(&mut headers);
        if request.parse(&buffer[..length])?.is_complete() {
            return Ok(request
                .headers
                .iter()
                .find(|header| header.name.eq_ignore_ascii_case("origin"))
                .map(|header| String::from_utf8_lossy(header.value).into_owned()));
        }
        if length == buffer.len() {
            bail!("handshake headers too large");
        }
        if length == seen {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        seen = length;
    }
}

async fn pump(
    websocket: tokio_tungstenite::WebSocketStream<TcpStream>,
    mux: UnixStream,
    closed: &mut tokio::sync::oneshot::Receiver<()>,
) -> anyhow::Result<()> {
    let (mut ws_tx, mut ws_rx) = websocket.split();
    let (mut mux_rx, mut mux_tx) = mux.into_split();

    let upstream = async {
        while let Some(message) = ws_rx.next().await {
            match message? {
                Message::Binary(data) => mux_tx.write_all(&data).await?,
                Message::Close(_) => break,
                _ => {}
            }
        }
        anyhow::Ok(())
    };
    let downstream = async {
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            let length = mux_rx.read(&mut buffer).await?;
            if length == 0 {
                break;
            }
            ws_tx
                .send(Message::Binary(buffer[..length].to_vec().into()))
                .await?;
        }
        ws_tx.close().await.ok();
        anyhow::Ok(())
    };

    tokio::select! {
        result = upstream => result,
        result = downstream => result,
        _ = closed => Ok(()),
    }
}
