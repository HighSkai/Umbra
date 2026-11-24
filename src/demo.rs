//! `umbra demo`: a self-contained preview with a simulated household.
//!
//! Nothing touches the network: checks are simulated, no alerts are sent,
//! Ring/Blink stay off, Wake-on-LAN and traceroute are inert, and the server
//! only listens on 127.0.0.1 with no password. On first start it backfills 24
//! hours of history (a laptop that sleeps overnight, a short internet outage at
//! night, a phone on weak Wi-Fi) so the map, sparklines and heatmap have
//! something to show. Used for the README screenshots.

use crate::config::{Config, Device, Kind};
use crate::db::{CheckRow, Db};
use crate::util::now;
use chrono::{Local, TimeZone, Timelike};

pub const PATH: &str = "umbra-demo.toml";

pub fn config() -> Config {
    let mut c = Config::default();
    c.server.name = "Demo home".into();
    c.server.bind = "127.0.0.1:8770".into();
    c.server.data_dir = "umbra-demo-data".into();
    c.server.helpers_dir = "helpers".into();
    c.network.router = "192.168.1.1".into();
    let dev = |id: &str, name: &str, kind: Kind, host: &str| Device { id: id.into(), name: name.into(), kind, host: host.into(), ping: true, ..Default::default() };
    let mut desk = dev("desk-pc", "Desk PC", Kind::Computer, "192.168.1.20");
    desk.mac = Some("02:00:5E:10:00:20".into());
    let mut nas = dev("nas", "NAS", Kind::Server, "192.168.1.30");
    nas.tcp_port = Some(445);
    c.devices = vec![
        desk,
        dev("work-laptop", "Work laptop", Kind::Computer, "192.168.1.21"),
        nas,
        dev("front-camera", "Front camera", Kind::Camera, "192.168.1.60"),
        dev("garage-camera", "Garage camera", Kind::Camera, "192.168.1.61"),
        dev("living-room-tv", "Living room TV", Kind::Tv, "192.168.1.40"),
        dev("console", "Game console", Kind::Console, "192.168.1.41"),
        dev("phone", "Phone", Kind::Phone, "192.168.1.70"),
        dev("printer", "Printer", Kind::Printer, "192.168.1.50"),
    ];
    c
}

fn noise(id: &str, t: i64) -> f64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in id.bytes().chain(t.to_le_bytes()) {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    (h >> 11) as f64 / (1u64 << 53) as f64
}

fn minute_of_day(t: i64) -> u32 {
    Local.timestamp_opt(t, 0).single().map(|d| d.hour() * 60 + d.minute()).unwrap_or(0)
}

fn between(m: u32, from: u32, to: u32) -> bool {
    if from <= to { m >= from && m < to } else { m >= from || m < to }
}

/// One simulated check: (answered, latency ms).
pub fn simulate(id: &str, kind: Option<Kind>, t: i64) -> (bool, Option<f64>) {
    let m = minute_of_day(t);
    let wave = (t as f64 / 5400.0 + noise(id, 0) * 6.0).sin();
    let n = noise(id, t);
    let ok = |base: f64, spread: f64| (true, Some(base * (1.0 + 0.18 * wave) + spread * n));
    match (id, kind) {
        ("router", _) => ok(1.6, 1.2),
        ("internet", _) if between(m, 3 * 60 + 12, 3 * 60 + 29) => (false, None),
        ("internet", _) => ok(if between(m, 19 * 60, 23 * 60) { 19.0 } else { 13.0 }, 6.0),
        ("work-laptop", _) if between(m, 23 * 60 + 30, 8 * 60 + 10) => (false, None),
        ("phone", _) if between(m, 1 * 60, 6 * 60) && n < 0.35 => (false, None),
        ("phone", _) => ok(170.0, 70.0),
        (_, Some(Kind::Camera)) if n < 0.01 => (false, None),
        (_, Some(Kind::Camera)) => ok(6.5, 5.0),
        (_, Some(Kind::Tv | Kind::Console)) => ok(3.2, 2.5),
        _ => ok(1.9, 1.4),
    }
}

/// Fill an empty database with 24 hours of simulated history and its outages.
pub fn backfill(db: &Db, cfg: &Config) {
    if db.buckets_24h().1.values().any(|b| b.iter().any(|x| x.n > 0)) {
        return;
    }
    let end = now();
    let start = end - 86_400;
    let mut targets: Vec<(String, Option<Kind>, i64)> = vec![("router".into(), None, cfg.network.router_interval as i64), ("internet".into(), None, cfg.network.internet_interval as i64)];
    targets.extend(cfg.devices.iter().map(|d| (d.id.clone(), Some(d.kind), d.interval() as i64)));
    let mut rows = vec![];
    for (id, kind, step) in &targets {
        let mut t = start;
        let mut down_since: Option<i64> = None;
        let mut fails = 0;
        while t < end {
            let (ok, ms) = simulate(id, *kind, t);
            rows.push(CheckRow { target: id.clone(), ts: t, ok, ms });
            if ok {
                if let Some(s) = down_since.take() {
                    let oid = db.open_outage(id, s);
                    db.close_outage(oid, t);
                }
                fails = 0;
            } else {
                fails += 1;
                if fails == 3 {
                    down_since = Some(t - 2 * step);
                }
            }
            t += step;
        }
        if let Some(s) = down_since {
            db.open_outage(id, s);
        }
    }
    db.insert_checks(&rows);
    crate::info!("demo: backfilled {} simulated checks", rows.len());
}
