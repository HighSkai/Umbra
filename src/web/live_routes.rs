//! HTTP side of live view (see crate::live for the design).

use crate::live;
use crate::state::Shared;
use axum::Json;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio_stream::Stream;
use tokio_util::io::ReaderStream;

fn err(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(json!({ "error": msg.into() }))).into_response()
}

pub async fn start(State(app): State<Shared>, Path((source, id)): Path<(String, String)>) -> Response {
    if source != "ring" && source != "blink" {
        return err(StatusCode::NOT_FOUND, "Unknown source");
    }
    match live::start(&app, &source, &id).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => err(StatusCode::BAD_GATEWAY, e),
    }
}

#[derive(Deserialize)]
pub struct Offer {
    sdp: String,
}

pub async fn offer(State(app): State<Shared>, Path(sid): Path<String>, Json(o): Json<Offer>) -> Response {
    match live::send(&app, &sid, json!({ "type": "offer", "sdp": o.sdp })).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => err(StatusCode::GONE, e),
    }
}

#[derive(Deserialize)]
pub struct Candidate {
    candidate: String,
    mline: i64,
}

pub async fn candidate(State(app): State<Shared>, Path(sid): Path<String>, Json(c): Json<Candidate>) -> Response {
    match live::send(&app, &sid, json!({ "type": "candidate", "candidate": c.candidate, "mline": c.mline })).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => err(StatusCode::GONE, e),
    }
}

pub async fn keepalive(State(app): State<Shared>, Path(sid): Path<String>) -> Response {
    match live::send(&app, &sid, json!({ "type": "keepalive" })).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => err(StatusCode::GONE, e),
    }
}

pub async fn stop(State(app): State<Shared>, Path(sid): Path<String>) -> Response {
    live::stop(&app, &sid).await;
    StatusCode::NO_CONTENT.into_response()
}

/// Ring signaling messages (answer, ICE candidates, close) for one session.
pub async fn events(State(app): State<Shared>, Path(sid): Path<String>) -> Response {
    let Some(rx) = live::take_events(&app, &sid).await else {
        return err(StatusCode::GONE, "This live view has ended");
    };
    let stream = tokio_stream::wrappers::ReceiverStream::new(rx).map_ev();
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(20))).into_response()
}

trait MapEv {
    fn map_ev(self) -> impl Stream<Item = Result<Event, std::convert::Infallible>>;
}

impl MapEv for tokio_stream::wrappers::ReceiverStream<serde_json::Value> {
    fn map_ev(self) -> impl Stream<Item = Result<Event, std::convert::Infallible>> {
        use tokio_stream::StreamExt;
        self.map(|v| {
            let kind = v["type"].as_str().unwrap_or("message").to_string();
            Ok(Event::default().event(kind).data(v.to_string()))
        })
    }
}

/// Owns ffmpeg for as long as the browser keeps the response open.
struct ChildStream {
    _child: tokio::process::Child,
    inner: ReaderStream<tokio::process::ChildStdout>,
}

impl Stream for ChildStream {
    type Item = std::io::Result<axum::body::Bytes>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }
}

/// Blink: fragmented MP4 for Media Source Extensions.
pub async fn stream(State(app): State<Shared>, Path(sid): Path<String>) -> Response {
    let Some(url) = live::stream_url(&app, &sid).await else {
        return err(StatusCode::GONE, "This live view has ended");
    };
    let mut child = match live::ffmpeg(&app, &url) {
        Ok(c) => c,
        Err(e) => return err(StatusCode::SERVICE_UNAVAILABLE, e),
    };
    let Some(out) = child.stdout.take() else { return err(StatusCode::INTERNAL_SERVER_ERROR, "ffmpeg has no output") };
    let body = Body::from_stream(ChildStream { _child: child, inner: ReaderStream::with_capacity(out, 32 * 1024) });
    let mut r = Response::new(body);
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("video/mp4"));
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}
