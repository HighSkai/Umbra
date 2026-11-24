//! State-change alerts to the phone (ntfy) and/or Discord.
//!
//! Alerts are queued in SQLite and sent by one loop. While the internet is down
//! nothing is attempted; when it returns the queue drains. If the matching
//! "down" alert never got out, it is folded into a single "was down 14:02-14:20"
//! message instead of two stale ones. Delivery uses the system `curl`, with the
//! request (including the secret URL/topic) passed on stdin, never argv.

use crate::state::{App, Shared, Status, TargetKind};
use crate::util::{fmt_clock, fmt_dur, now};
use std::time::Duration;
use tokio::io::AsyncWriteExt;

fn what(kind: TargetKind, name: &str) -> String {
    match kind {
        TargetKind::Router => "Router".into(),
        TargetKind::Internet => "Internet".into(),
        TargetKind::Device(_) => name.to_string(),
    }
}

pub fn down(app: &App, key: &str, name: &str, kind: TargetKind, started: i64) {
    let w = what(kind, name);
    let body = match kind {
        TargetKind::Internet => format!("No response from the internet since {}. The router is still up.", fmt_clock(started)),
        TargetKind::Router => format!("The router stopped answering at {}.", fmt_clock(started)),
        TargetKind::Device(_) => format!("Not responding since {}.", fmt_clock(started)),
    };
    app.db.insert_alert(&format!("{w} is down"), &body, 4, "down", Some(key));
    app.alert_wake.notify_one();
}

pub fn up(app: &App, key: &str, name: &str, kind: TargetKind, started: i64, ended: i64) {
    let w = what(kind, name);
    let span = format!("{}–{}", fmt_clock(started), fmt_clock(ended));
    let dur = fmt_dur(ended - started);
    if app.db.take_unsent_down(key).is_some() {
        app.db.insert_alert(&format!("{w} was down for {dur}"), &format!("{span}. It's back now."), 3, "up", Some(key));
    } else {
        app.db.insert_alert(&format!("{w} is back"), &format!("Was down for {dur} ({span})."), 3, "up", Some(key));
    }
    app.alert_wake.notify_one();
}

pub fn power_out(app: &App, started: i64, battery: Option<f64>) {
    let b = battery.map(|b| format!(" Battery at {b:.0}%.")).unwrap_or_default();
    app.db.insert_alert("Power outage at home", &format!("The server switched to battery at {}.{b}", fmt_clock(started)), 5, "down", Some("power"));
    app.alert_wake.notify_one();
}

pub fn power_back(app: &App, started: i64, ended: i64) {
    let span = format!("{}–{}", fmt_clock(started), fmt_clock(ended));
    let dur = fmt_dur(ended - started);
    if app.db.take_unsent_down("power").is_some() {
        app.db.insert_alert(&format!("Power was out for {dur}"), &format!("{span}. Power is back."), 4, "up", Some("power"));
    } else {
        app.db.insert_alert("Power restored", &format!("Power was out for {dur} ({span})."), 4, "up", Some("power"));
    }
    app.alert_wake.notify_one();
}

pub fn info(app: &App, title: &str, body: &str, priority: i64) {
    app.db.insert_alert(title, body, priority, "info", None);
    app.alert_wake.notify_one();
}

struct Channels {
    ntfy: Option<(String, String, Option<String>)>,
    discord: Option<String>,
}

fn channels(app: &App) -> Channels {
    let cfg = app.cfg().alerts.clone();
    let ntfy = if cfg.ntfy {
        app.secrets.get("ntfy_topic").filter(|t| !t.is_empty()).map(|t| (cfg.ntfy_server.trim_end_matches('/').to_string(), t, app.secrets.get("ntfy_token").filter(|s| !s.is_empty())))
    } else {
        None
    };
    let discord = if cfg.discord { app.secrets.get("discord_webhook").filter(|u| u.starts_with("https://")) } else { None };
    Channels { ntfy, discord }
}

pub fn configured(app: &App) -> bool {
    let c = channels(app);
    c.ntfy.is_some() || c.discord.is_some()
}

pub async fn sender_loop(app: Shared) {
    if app.demo {
        return; // the demo never sends anything
    }
    loop {
        tokio::select! {
            _ = app.alert_wake.notified() => {}
            _ = tokio::time::sleep(Duration::from_secs(60)) => {}
        }
        // Give a burst of state changes a moment to settle (and to collapse).
        tokio::time::sleep(Duration::from_secs(3)).await;
        let ch = channels(&app);
        if ch.ntfy.is_none() && ch.discord.is_none() {
            continue;
        }
        let internet_down = app.targets().iter().any(|t| t.id == "internet" && t.status == Status::Down);
        if internet_down {
            continue;
        }
        for a in app.db.due_alerts(10) {
            let t = now();
            if t - a.created > 86_400 {
                app.db.mark_alert(a.id, Some(-1), t); // too old to be useful
                continue;
            }
            let body = if t - a.created > 180 { format!("{}\n(Held from {} until the server could reach the internet.)", a.body, fmt_clock(a.created)) } else { a.body.clone() };
            let mut delivered = false;
            if let Some((server, topic, token)) = &ch.ntfy {
                match send_ntfy(server, topic, token.as_deref(), &a.title, &body, a.priority).await {
                    Ok(()) => delivered = true,
                    Err(e) => crate::warn!("alert via ntfy failed: {e}"),
                }
            }
            if let Some(url) = &ch.discord {
                match send_discord(url, &a.title, &body, a.priority, a.created).await {
                    Ok(()) => delivered = true,
                    Err(e) => crate::warn!("alert via Discord failed: {e}"),
                }
            }
            if delivered {
                app.db.mark_alert(a.id, Some(t), t);
            } else {
                let backoff = (30i64 << a.attempts.clamp(0, 7)).min(3600);
                app.db.mark_alert(a.id, None, t + backoff);
            }
        }
    }
}

/// Sends a test message right away; returns a line per channel.
pub async fn send_test(app: &App) -> Vec<(String, Result<(), String>)> {
    if app.demo {
        return vec![("Demo".into(), Err("the demo doesn't send alerts".into()))];
    }
    let ch = channels(app);
    let mut out = vec![];
    let title = "Umbra test alert";
    let body = format!("Alerts from {} work. Sent at {}.", app.cfg().server.name, fmt_clock(now()));
    if let Some((server, topic, token)) = &ch.ntfy {
        out.push(("Phone (ntfy)".into(), send_ntfy(server, topic, token.as_deref(), title, &body, 3).await));
    }
    if let Some(url) = &ch.discord {
        out.push(("Discord".into(), send_discord(url, title, &body, 3, now()).await));
    }
    out
}

fn q(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '\\' => o.push_str("\\\\"),
            '"' => o.push_str("\\\""),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

async fn send_ntfy(server: &str, topic: &str, token: Option<&str>, title: &str, body: &str, priority: i64) -> Result<(), String> {
    let tags = match priority {
        5 => vec!["zap"],
        4 => vec!["warning"],
        _ => vec!["white_check_mark"],
    };
    let json = serde_json::json!({ "topic": topic, "title": title, "message": body, "priority": priority, "tags": tags });
    let mut cfg = format!("url = {}\nrequest = \"POST\"\nheader = \"Content-Type: application/json\"\ndata = {}\n", q(&format!("{server}/")), q(&json.to_string()));
    if let Some(tok) = token {
        cfg.push_str(&format!("header = {}\n", q(&format!("Authorization: Bearer {tok}"))));
    }
    curl(cfg).await
}

async fn send_discord(url: &str, title: &str, body: &str, priority: i64, ts: i64) -> Result<(), String> {
    let color = match priority {
        5 => 0xF07A5F,
        4 => 0xF5A524,
        _ => 0x2FD6A6,
    };
    let iso = chrono::DateTime::from_timestamp(ts, 0).map(|d| d.to_rfc3339()).unwrap_or_default();
    let json = serde_json::json!({
        "username": "Umbra",
        "allowed_mentions": { "parse": [] },
        "embeds": [{ "title": title, "description": body, "color": color, "timestamp": iso }]
    });
    let cfg = format!("url = {}\nrequest = \"POST\"\nheader = \"Content-Type: application/json\"\ndata = {}\n", q(url), q(&json.to_string()));
    curl(cfg).await
}

async fn curl(config: String) -> Result<(), String> {
    let mut child = tokio::process::Command::new("curl")
        .args(["-sS", "-f", "--max-time", "20", "-A", "Umbra", "-o", if cfg!(windows) { "NUL" } else { "/dev/null" }, "-K", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("can't run curl ({e}); install curl on the server"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(config.as_bytes()).await.map_err(|e| e.to_string())?;
    }
    let out = tokio::time::timeout(Duration::from_secs(30), child.wait_with_output()).await.map_err(|_| "timed out".to_string())?.map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(if err.is_empty() { format!("curl exited with {}", out.status) } else { err })
    }
}
