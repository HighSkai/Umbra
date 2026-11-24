//! On-demand live view. Nothing streams unless you press "Live", and every
//! session ends when the tab is hidden or closed, when keepalives stop, or after
//! `live.max_minutes`.
//!
//! * Ring: WebRTC. The browser talks media directly to Ring's servers; Umbra only
//!   relays signaling (offer/answer/ICE) through the Python helper. Video never
//!   passes through the server.
//! * Blink: Blink's own "immis" stream (MPEG-TS) is pulled by the helper and
//!   repackaged by ffmpeg into fragmented MP4 (`-c:v copy`, no re-encode) for the
//!   browser's Media Source Extensions.

use crate::integrations::{self, Helper};
use crate::state::{App, Shared};
use serde_json::{Value, json};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

pub struct LiveSession {
    pub source: String,
    pub camera: String,
    helper: Helper,
    started: Instant,
    last_seen: Instant,
    events: Option<mpsc::Receiver<Value>>,
    tcp_url: Option<String>,
}

const MAX_SESSIONS: usize = 3;

pub async fn start(app: &Shared, source: &str, camera: &str) -> Result<Value, String> {
    if app.demo {
        return Err("Live view isn't available in the demo".into());
    }
    if !integrations::connected(app, source) {
        return Err(format!("{} isn't connected", integrations::title(source)));
    }
    let known = app.integ.lock().map(|s| s.get(source).cameras.iter().any(|c| c.id == camera)).unwrap_or(false);
    if !known {
        return Err("Unknown camera".into());
    }
    // One viewer per camera; starting again replaces the old session.
    let old: Vec<String> = app.live.lock().await.iter().filter(|(_, s)| s.source == source && s.camera == camera).map(|(k, _)| k.clone()).collect();
    for sid in old {
        stop(app, &sid).await;
    }
    if app.live.lock().await.len() >= MAX_SESSIONS {
        return Err("Too many live views open. Close one first.".into());
    }
    let input = integrations::input_for_live(app, source, camera).ok_or("Not connected")?;
    let mut helper = Helper::spawn(app, source, "live", &input).await?;
    let ready = match helper.recv(app, Duration::from_secs(40)).await {
        Ok(v) if v["type"] == "ready" => v,
        Ok(v) => {
            let msg = v["message"].as_str().unwrap_or("Live view failed to start").to_string();
            helper.finish(app, false, &msg).await;
            return Err(msg);
        }
        Err(e) => {
            helper.finish(app, false, &e).await;
            return Err(e);
        }
    };
    let sid = crate::util::token();
    let mut session = LiveSession { source: source.into(), camera: camera.into(), helper, started: Instant::now(), last_seen: Instant::now(), events: None, tcp_url: None };
    let reply = if source == "ring" {
        // Forward every later helper line (answer, ICE candidates, close) to the browser.
        let (tx, rx) = mpsc::channel(64);
        if let Some(mut out) = session.helper.out.take() {
            tokio::spawn(async move {
                while let Ok(Some(line)) = out.next_line().await {
                    if let Ok(v) = serde_json::from_str::<Value>(&line)
                        && v["type"] != "token" && tx.send(v).await.is_err() {
                            break;
                        }
                }
                let _ = tx.send(json!({ "type": "closed" })).await;
            });
        }
        session.events = Some(rx);
        json!({ "session": sid, "kind": "webrtc", "ice_servers": ready["ice_servers"].clone() })
    } else {
        session.tcp_url = ready["url"].as_str().map(str::to_string);
        if let Some(mut out) = session.helper.out.take() {
            tokio::spawn(async move { while let Ok(Some(_)) = out.next_line().await {} });
        }
        json!({ "session": sid, "kind": "mse", "stream": format!("/live/s/{sid}/stream.mp4") })
    };
    app.live.lock().await.insert(sid, session);
    Ok(reply)
}

pub async fn send(app: &App, sid: &str, msg: Value) -> Result<(), String> {
    let mut map = app.live.lock().await;
    let s = map.get_mut(sid).ok_or("This live view has ended")?;
    s.last_seen = Instant::now();
    s.helper.send(&msg).await
}

pub async fn take_events(app: &App, sid: &str) -> Option<mpsc::Receiver<Value>> {
    app.live.lock().await.get_mut(sid)?.events.take()
}

pub async fn stream_url(app: &App, sid: &str) -> Option<String> {
    let mut map = app.live.lock().await;
    let s = map.get_mut(sid)?;
    s.last_seen = Instant::now();
    s.tcp_url.clone()
}

pub async fn stop(app: &App, sid: &str) {
    let session = app.live.lock().await.remove(sid);
    if let Some(mut s) = session {
        let _ = s.helper.send(&json!({ "type": "close" })).await;
        let note = format!("live {} s", s.started.elapsed().as_secs());
        s.helper.finish(app, true, &note).await;
    }
}

pub async fn sessions(app: &App) -> Vec<(String, String, u64)> {
    app.live.lock().await.values().map(|s| (s.source.clone(), s.camera.clone(), s.started.elapsed().as_secs())).collect()
}

/// Ends sessions that ran too long or whose tab went away.
pub async fn reaper_loop(app: Shared) {
    loop {
        tokio::time::sleep(Duration::from_secs(15)).await;
        let max = Duration::from_secs(app.cfg().live.max_minutes.clamp(1, 60) * 60);
        let expired: Vec<String> = app
            .live
            .lock()
            .await
            .iter()
            .filter(|(_, s)| s.started.elapsed() > max || s.last_seen.elapsed() > Duration::from_secs(75))
            .map(|(k, _)| k.clone())
            .collect();
        for sid in expired {
            stop(&app, &sid).await;
        }
    }
}

/// Starts ffmpeg for a Blink session: MPEG-TS in, fragmented MP4 out, video copied.
pub fn ffmpeg(app: &App, url: &str) -> Result<tokio::process::Child, String> {
    let bin = app.cfg().live.ffmpeg.clone();
    tokio::process::Command::new(&bin)
        .args([
            "-hide_banner", "-loglevel", "error", "-fflags", "+nobuffer+genpts", "-probesize", "500000", "-analyzeduration", "1000000",
            "-i", url, "-map", "0:v:0", "-map", "0:a:0?", "-c:v", "copy", "-c:a", "aac", "-b:a", "64k",
            "-f", "mp4", "-movflags", "frag_keyframe+empty_moov+default_base_moof", "-frag_duration", "500000", "pipe:1",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("Blink live view needs ffmpeg on the server ({bin}: {e})"))
}
