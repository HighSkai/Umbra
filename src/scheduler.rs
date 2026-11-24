//! One lightweight task per target. Healthy targets are checked at their interval;
//! a failure is re-checked quickly to confirm, and a target that is down backs off
//! exponentially so a dead device costs almost nothing.

use crate::alerts;
use crate::db::CheckRow;
use crate::net::{self, icmp};
use crate::state::{LiveEvent, Shared, Status, TargetKind};
use crate::util::now;
use std::time::Duration;

#[derive(Clone)]
struct Spec {
    id: String,
    mode: Mode,
    interval: u64,
    timeout: Duration,
    tcp_port: Option<u16>,
    ping: bool,
    max_backoff: u64,
}

#[derive(Clone)]
enum Mode {
    Router(String),
    Internet(Vec<String>),
    Device(String),
}

#[derive(Default)]
struct Extra {
    server_offline: Option<bool>,
    resolved: Option<String>,
    tcp_ok: Option<bool>,
}

/// (Re)start every check task from the current config.
pub fn restart(app: &Shared) {
    app.rebuild_targets();
    let cfg = app.cfg().clone();
    let timeout = Duration::from_millis(cfg.network.ping_timeout_ms.clamp(200, 5000));
    let mut specs = vec![
        Spec { id: "router".into(), mode: Mode::Router(cfg.network.router.clone()), interval: cfg.network.router_interval.max(5), timeout, tcp_port: None, ping: true, max_backoff: 60 },
        Spec { id: "internet".into(), mode: Mode::Internet(cfg.network.internet_targets.clone()), interval: cfg.network.internet_interval.max(5), timeout, tcp_port: None, ping: true, max_backoff: 60 },
    ];
    for d in &cfg.devices {
        if !d.ping && d.tcp_port.is_none() {
            continue;
        }
        specs.push(Spec { id: d.id.clone(), mode: Mode::Device(d.host.clone()), interval: d.interval(), timeout, tcp_port: d.tcp_port, ping: d.ping, max_backoff: 900 });
    }
    let mut tasks = app.tasks.lock().unwrap_or_else(|e| e.into_inner());
    for t in tasks.drain(..) {
        t.abort();
    }
    for (i, spec) in specs.into_iter().enumerate() {
        tasks.push(tokio::spawn(run(app.clone(), spec, i)));
    }
    drop(tasks);
    app.changed();
}

async fn run(app: Shared, spec: Spec, index: usize) {
    // Router and internet start right away; devices are spread over ~20 s.
    let first = if matches!(spec.mode, Mode::Device(_)) { 800 + (index as u64 * 1300) % 20_000 } else { 150 * index as u64 };
    tokio::time::sleep(Duration::from_millis(first)).await;
    loop {
        let (ok, ms, extra) = check(&app, &spec).await;
        let delay = record(&app, &spec, ok, ms, extra);
        tokio::time::sleep(Duration::from_secs(delay)).await;
    }
}

async fn ping_twice(ip: std::net::Ipv4Addr, timeout: Duration) -> Option<f64> {
    // One quiet retry absorbs a single dropped Wi-Fi packet.
    match icmp::ping(ip, timeout).await {
        Some(ms) => Some(ms),
        None => icmp::ping(ip, timeout).await,
    }
}

async fn check(app: &Shared, spec: &Spec) -> (bool, Option<f64>, Extra) {
    if app.demo {
        let kind = app.cfg().device(&spec.id).map(|d| d.kind);
        let (ok, ms) = crate::demo::simulate(&spec.id, kind, now());
        let host = match &spec.mode {
            Mode::Router(h) | Mode::Device(h) => Some(h.clone()),
            Mode::Internet(_) => None,
        };
        return (ok, ms, Extra { server_offline: Some(false), resolved: host, tcp_ok: spec.tcp_port.map(|_| ok) });
    }
    match &spec.mode {
        Mode::Router(host) => {
            let gw = net::default_gateway();
            *app.gateway.lock().unwrap_or_else(|e| e.into_inner()) = gw;
            let ip = if host == "auto" { gw } else { net::resolve(host).await };
            let offline = gw.is_none();
            match ip {
                None => (false, None, Extra { server_offline: Some(offline), ..Default::default() }),
                Some(ip) => {
                    let ms = ping_twice(ip, spec.timeout).await;
                    (ms.is_some(), ms, Extra { server_offline: Some(offline), resolved: Some(ip.to_string()), ..Default::default() })
                }
            }
        }
        Mode::Internet(targets) => {
            for t in targets {
                if let Some(ip) = net::resolve(t).await
                    && let Some(ms) = icmp::ping(ip, spec.timeout).await {
                        return (true, Some(ms), Extra::default());
                    }
            }
            (false, None, Extra::default())
        }
        Mode::Device(host) => {
            let Some(ip) = net::resolve(host).await else {
                return (false, None, Extra::default());
            };
            let mut ms = None;
            if spec.ping {
                ms = ping_twice(ip, spec.timeout).await;
            }
            let mut tcp_ok = None;
            if let Some(port) = spec.tcp_port {
                let t = net::tcp_check(ip, port, spec.timeout).await;
                tcp_ok = Some(t.is_some());
                if ms.is_none() {
                    ms = t; // ICMP blocked but the port answers: still up
                }
            }
            (ms.is_some(), ms, Extra { resolved: Some(ip.to_string()), tcp_ok, ..Default::default() })
        }
    }
}

fn record(app: &Shared, spec: &Spec, ok: bool, ms: Option<f64>, extra: Extra) -> u64 {
    let t = now();
    let loss_limit = app.cfg().network.degraded_loss_pct;
    app.stats.count_check();
    app.push_check(CheckRow { target: spec.id.clone(), ts: t, ok, ms });

    let (applied, prev_since, name, consec, down_after, kind) = {
        let mut ts = app.targets();
        let Some(tg) = ts.iter_mut().find(|x| x.id == spec.id) else { return spec.interval };
        if let Some(o) = extra.server_offline {
            tg.server_offline = o;
        }
        if extra.resolved.is_some() {
            tg.resolved = extra.resolved;
        }
        if extra.tcp_ok.is_some() {
            tg.tcp_ok = extra.tcp_ok;
        }
        let prev_since = tg.since;
        let a = tg.apply(ok, ms, t, loss_limit);
        (a, prev_since, tg.name.clone(), tg.consec_fail, tg.down_after, tg.kind)
    };
    app.emit(LiveEvent::Check { target: spec.id.clone(), ok, ms });

    if applied.prev != applied.new {
        if applied.new == Status::Down {
            went_down(app, &spec.id, &name, kind);
        } else if applied.prev == Status::Down {
            came_back(app, &spec.id, &name, kind, prev_since, t);
        }
        app.changed();
    }

    if consec == 0 {
        spec.interval
    } else if consec < down_after {
        spec.interval.min(15)
    } else {
        let n = (consec - down_after).min(6);
        (spec.interval << n).min(spec.max_backoff.max(spec.interval))
    }
}

fn went_down(app: &Shared, id: &str, name: &str, kind: TargetKind) {
    let started = app.targets().iter().find(|x| x.id == id).map(|x| x.since).unwrap_or_else(now);
    let oid = app.db.open_outage(id, started);
    let ov = app.overview();
    let cfg = app.cfg().alerts.clone();
    // Don't flood: when the router or power is out, everything behind it is "down" too.
    let allowed = match kind {
        TargetKind::Router => cfg.on_internet && !ov.server_offline && !ov.on_battery,
        TargetKind::Internet => cfg.on_internet && ov.router != Status::Down && !ov.server_offline && !ov.on_battery,
        TargetKind::Device(_) => cfg.on_devices && ov.router != Status::Down && !ov.server_offline && !ov.on_battery,
    };
    if allowed {
        alerts::down(app, id, name, kind, started);
    }
    if let Some(tg) = app.targets().iter_mut().find(|x| x.id == id) {
        tg.outage = Some(oid);
        tg.alerted = allowed;
    }
}

fn came_back(app: &Shared, id: &str, name: &str, kind: TargetKind, started: i64, ended: i64) {
    let (oid, alerted) = {
        let mut ts = app.targets();
        match ts.iter_mut().find(|x| x.id == id) {
            Some(tg) => (tg.outage.take(), std::mem::replace(&mut tg.alerted, false)),
            None => (None, false),
        }
    };
    if let Some(oid) = oid {
        app.db.close_outage(oid, ended);
    }
    if alerted {
        alerts::up(app, id, name, kind, started, ended);
    }
}

/// Writes buffered check results every 30 s (one transaction, few disk wakeups).
pub async fn flush_loop(app: Shared) {
    loop {
        tokio::time::sleep(Duration::from_secs(30)).await;
        app.flush_checks();
    }
}

/// Hourly rollups and pruning.
pub async fn maintenance_loop(app: Shared) {
    tokio::time::sleep(Duration::from_secs(90)).await;
    loop {
        app.flush_checks();
        let a = app.clone();
        let _ = tokio::task::spawn_blocking(move || a.db.maintain()).await;
        tokio::time::sleep(Duration::from_secs(3600)).await;
    }
}
