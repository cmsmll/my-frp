//! Local WebSocket echo and SSE service for testing TCP tunnels.
//!
//! Run with `cargo run --example stream_service -- 127.0.0.1:8080`.

use axum::{
    Json, Router,
    extract::{
        Query, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{HeaderMap, StatusCode},
    response::{
        Html, IntoResponse, Sse,
        sse::{Event, KeepAlive},
    },
    routing::get,
};
use futures_util::{SinkExt, stream};
use serde::Deserialize;
use serde_json::json;
use std::{convert::Infallible, time::Duration};
use tokio::time::{Instant, Interval, MissedTickBehavior};

/// The same router is used by the runnable example and integration tests.
pub fn app() -> Router {
    Router::new()
        .route(
            "/",
            get(|| async { Html(include_str!("stream_service.html")) }),
        )
        .route("/health", get(health))
        .route("/ws", get(websocket))
        .route("/sse", get(events))
}

async fn health() -> impl IntoResponse {
    Json(json!({"service": "my-frp-stream-demo", "status": "ok"}))
}

async fn websocket(upgrade: WebSocketUpgrade) -> impl IntoResponse {
    upgrade.on_upgrade(echo)
}

async fn echo(mut socket: WebSocket) {
    while let Some(Ok(message)) = socket.recv().await {
        match message {
            Message::Text(_) | Message::Binary(_) => {
                if socket.send(message).await.is_err() {
                    break;
                }
            }
            Message::Ping(_) => {
                // Axum queues the matching Pong; flush it without creating a duplicate.
                if socket.flush().await.is_err() {
                    break;
                }
            }
            Message::Pong(_) => {}
            Message::Close(_) => {
                // Axum also queues the matching close response automatically.
                let _ = socket.flush().await;
                break;
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EventQuery {
    count: Option<u64>,
    #[serde(default = "default_interval")]
    interval_ms: u64,
    #[serde(default)]
    tag: String,
}

fn default_interval() -> u64 {
    1_000
}

struct EventState {
    ticker: Interval,
    next: Option<u64>,
    remaining: Option<u64>,
    tag: String,
}

type BadRequest = (StatusCode, &'static str);

async fn events(
    Query(query): Query<EventQuery>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, BadRequest> {
    if !(100..=60_000).contains(&query.interval_ms) {
        return Err((StatusCode::BAD_REQUEST, "interval_ms must be 100..60000"));
    }
    if query.count == Some(0) {
        return Err((StatusCode::BAD_REQUEST, "count must be greater than zero"));
    }
    if query.tag.chars().count() > 128 {
        return Err((
            StatusCode::BAD_REQUEST,
            "tag must be at most 128 characters",
        ));
    }
    let last_id = match headers.get("last-event-id") {
        Some(value) => value
            .to_str()
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or((
                StatusCode::BAD_REQUEST,
                "Last-Event-ID must be a nonnegative integer",
            ))?,
        None => 0,
    };
    let next = last_id
        .checked_add(1)
        .ok_or((StatusCode::BAD_REQUEST, "Last-Event-ID is too large"))?;
    let period = Duration::from_millis(query.interval_ms);
    // Delaying the first event makes incremental delivery observable in tests.
    let mut ticker = tokio::time::interval_at(Instant::now() + period, period);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let state = EventState {
        ticker,
        next: Some(next),
        remaining: query.count,
        tag: query.tag,
    };
    let events = stream::unfold(state, |mut state| async move {
        if state.remaining == Some(0) {
            return None;
        }
        let seq = state.next?;
        state.ticker.tick().await;
        state.next = seq.checked_add(1);
        state.remaining = state.remaining.map(|remaining| remaining - 1);
        let event = Event::default()
            .event("tick")
            .id(seq.to_string())
            .data(json!({"seq": seq, "tag": state.tag}).to_string());
        Some((Ok::<_, Infallible>(event), state))
    });
    let response = Sse::new(events).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("heartbeat"),
    );
    Ok(([("x-accel-buffering", "no")], response))
}

#[cfg(not(test))]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    use std::{future::IntoFuture, net::SocketAddr};

    let mut args = std::env::args().skip(1);
    let address = args.next().unwrap_or_else(|| "127.0.0.1:8080".into());
    if address == "--help" || address == "-h" {
        println!("Usage: stream_service [127.0.0.1:8080]");
        return Ok(());
    }
    anyhow::ensure!(
        args.next().is_none(),
        "Usage: stream_service [127.0.0.1:8080]"
    );
    let address: SocketAddr = address.parse()?;
    let listener = tokio::net::TcpListener::bind(address).await?;
    println!("WS / SSE demo: http://{}", listener.local_addr()?);
    println!("Endpoints: /health, /ws, /sse; press Ctrl+C to stop.");
    // Stop active streaming connections as well when the demo is interrupted.
    tokio::select! {
        result = axum::serve(listener, app()).into_future() => result?,
        signal = tokio::signal::ctrl_c() => signal?,
    }
    Ok(())
}
