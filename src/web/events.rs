//! Server-sent events for open dashboards. The page closes the stream whenever
//! the tab is hidden, so with no visible tab nothing is sent at all.

use crate::state::{LiveEvent, Shared};
use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use std::convert::Infallible;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio_stream::Stream;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

struct ClientGuard(Shared);

impl Drop for ClientGuard {
    fn drop(&mut self) {
        self.0.stats.sse_clients.fetch_sub(1, Ordering::Relaxed);
    }
}

pub async fn sse(State(app): State<Shared>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    app.stats.sse_clients.fetch_add(1, Ordering::Relaxed);
    let guard = ClientGuard(app.clone());
    let rx = app.events.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(move |msg| {
        let _keep = &guard;
        let ev = match msg {
            Ok(LiveEvent::Check { target, ok, ms }) => Event::default().event("check").data(serde_json::json!({ "t": target, "ok": ok, "ms": ms }).to_string()),
            Ok(LiveEvent::Changed { mood }) => Event::default().event("changed").data(serde_json::json!({ "mood": mood }).to_string()),
            // Fell behind: just ask the page to refresh.
            Err(_) => Event::default().event("changed").data("{}"),
        };
        Some(Ok(ev))
    });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(25)))
}
