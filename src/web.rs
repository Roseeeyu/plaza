//! The HTTP side, built on axum: the frontend page, a health check, and
//! WebSocket sessions at `/ws`.

use std::future;
use std::io;
use std::sync::Arc;

use axum::Router;
use axum::extract::State as AxumState;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use futures_util::{SinkExt, StreamExt, stream};
use tokio::net::TcpListener;

use crate::server::{MAX_LINE, State, run_session};

const INDEX_HTML: &str = include_str!("../web/index.html");

pub async fn serve(listener: TcpListener, state: Arc<State>) -> io::Result<()> {
    let app = Router::new()
        .route("/", get(|| async { Html(INDEX_HTML) }))
        .route("/healthz", get(|| async { "ok" }))
        .route("/ws", get(upgrade))
        .with_state(state);
    axum::serve(listener, app).await
}

async fn upgrade(
    ws: WebSocketUpgrade,
    AxumState(state): AxumState<Arc<State>>,
) -> impl IntoResponse {
    ws.max_message_size(MAX_LINE)
        .on_upgrade(move |socket| session(socket, state))
}

/// Adapts a WebSocket into the line streams [`run_session`] expects: each
/// text frame may carry one or more request lines, and each reply is sent
/// as one text frame.
async fn session(socket: WebSocket, state: Arc<State>) {
    let (sink, frames) = socket.split();

    let incoming = frames
        .take_while(|frame| {
            future::ready(matches!(frame, Ok(m) if !matches!(m, Message::Close(_))))
        })
        .filter_map(|frame| {
            future::ready(match frame {
                Ok(Message::Text(text)) => Some(text),
                _ => None,
            })
        })
        .flat_map(|text| {
            let lines: Vec<String> = text.as_str().lines().map(str::to_owned).collect();
            stream::iter(lines)
        });
    let outgoing =
        sink.with(|reply: String| future::ready(Ok::<_, axum::Error>(Message::text(reply))));

    run_session(incoming, outgoing, state).await;
}
