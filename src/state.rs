//! Shared application state and the up/degraded/down model.

use crate::config::{Config, Kind};
use crate::db::{Bucket, BUCKETS, CheckRow, Db};
use crate::secrets::Secrets;
use crate::util::now;
use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard};
use std::time::Instant;
use tokio::sync::{Notify, broadcast};

pub type Shared = Arc<App>;

pub struct App {
    pub cfg: RwLock<Config>,
    pub cfg_path: PathBuf,
    pub db: Db,
    pub secrets: Secrets,
    pub targets: Mutex<Vec<Target>>,
    pub pending: Mutex<Vec<CheckRow>>,
    pub events: broadcast::Sender<LiveEvent>,
    pub host: Mutex<crate::host::HostStatus>,
    pub integ: Mutex<crate::integrations::IntegState>,
    pub stats: crate::stats::Stats,
    pub alert_wake: Notify,
    pub ring_wake: Notify,
    pub blink_wake: Notify,
    pub tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    pub sessions: Mutex<HashMap<String, (String, i64)>>,
    pub login_fails: Mutex<HashMap<IpAddr, (u32, i64)>>,
    pub logins: tokio::sync::Mutex<HashMap<String, crate::integrations::LoginFlow>>,
    pub live: tokio::sync::Mutex<HashMap<String, crate::live::LiveSession>>,
    pub buckets_cache: Mutex<Option<(i64, Arc<(i64, HashMap<String, [Bucket; BUCKETS]>)>)>>,
    pub started: Instant,
    pub started_unix: i64,
    pub traceroute_busy: AtomicBool,
    pub gateway: Mutex<Option<std::net::Ipv4Addr>>,
    /// `umbra demo`: simulated checks, no outbound traffic, no password, loopback only.
    pub demo: bool,
}

impl App {
    pub fn new(cfg: Config, cfg_path: PathBuf, db: Db, secrets: Secrets, demo: bool) -> App {
        let (tx, _) = broadcast::channel(256);
        App {
            cfg: RwLock::new(cfg),
            cfg_path,
            db,
            secrets,
            targets: Mutex::new(vec![]),
            pending: Mutex::new(vec![]),
            events: tx,
            host: Mutex::new(Default::default()),
            integ: Mutex::new(Default::default()),
            stats: crate::stats::Stats::new(),
            alert_wake: Notify::new(),
            ring_wake: Notify::new(),
            blink_wake: Notify::new(),
            tasks: Mutex::new(vec![]),
            sessions: Mutex::new(HashMap::new()),
            login_fails: Mutex::new(HashMap::new()),
            logins: tokio::sync::Mutex::new(HashMap::new()),
            live: tokio::sync::Mutex::new(HashMap::new()),
            buckets_cache: Mutex::new(None),
            started: Instant::now(),
            started_unix: now(),
            traceroute_busy: AtomicBool::new(false),
            gateway: Mutex::new(None),
            demo,
        }
    }

    pub fn cfg(&self) -> RwLockReadGuard<'_, Config> {
        self.cfg.read().unwrap_or_else(|e| e.into_inner())
    }

    pub fn targets(&self) -> MutexGuard<'_, Vec<Target>> {
        self.targets.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn emit(&self, ev: LiveEvent) {
        let _ = self.events.send(ev);
    }

    /// Tell open dashboards that something visible changed.
    pub fn changed(&self) {
        let mood = self.overview().mood;
        self.emit(LiveEvent::Changed { mood });
    }

    /// Rebuild the target list from config, keeping live state for ids that remain.
    pub fn rebuild_targets(&self) {
        let cfg = self.cfg().clone();
        let mut old: HashMap<String, Target> = self.targets().drain(..).map(|t| (t.id.clone(), t)).collect();
        let open: HashMap<String, (i64, i64)> = self.db.open_outages().into_iter().map(|o| (o.target, (o.id, o.started))).collect();
        let mut list = vec![];
        let net = &cfg.network;
        let mut push = |mut t: Target| {
            if let Some(prev) = old.remove(&t.id) {
                t.adopt(prev);
            } else if let Some((id, started)) = open.get(&t.id) {
                t.outage = Some(*id);
                t.status = Status::Down;
                t.since = *started;
                t.first_fail = Some(*started);
                t.consec_fail = t.down_after;
            }
            list.push(t);
        };
        push(Target::new("router", "Router", TargetKind::Router, net.router.clone(), "router", net.down_after.min(3), net.degraded_ms.min(80.0), true));
        push(Target::new("internet", "Internet", TargetKind::Internet, net.internet_targets.join(", "), "globe", net.down_after.min(3), net.degraded_ms, true));
        for d in &cfg.devices {
            let mut t = Target::new(
                &d.id,
                &d.name,
                TargetKind::Device(d.kind),
                d.host.clone(),
                d.icon(),
                d.down_after.unwrap_or(d.kind.default_down_after(net.down_after)),
                d.degraded_ms.unwrap_or(net.degraded_ms),
                d.ping || d.tcp_port.is_some(),
            );
            t.mac = d.mac.clone();
            t.tcp_port = d.tcp_port;
            push(t);
        }
        // Outages for removed devices are closed so they don't stay open forever.
        for (id, (oid, _)) in open {
            if !list.iter().any(|t| t.id == id) && id != "power" {
                self.db.close_outage(oid, now());
            }
        }
        *self.targets() = list;
    }

    pub fn overview(&self) -> Overview {
        let on_battery = self.host.lock().map(|h| h.on_ac == Some(false)).unwrap_or(false);
        let battery = self.host.lock().ok().and_then(|h| h.battery_pct);
        overview(&self.targets(), on_battery, battery)
    }

    pub fn push_check(&self, row: CheckRow) {
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).push(row);
    }

    pub fn flush_checks(&self) {
        let rows: Vec<CheckRow> = std::mem::take(&mut *self.pending.lock().unwrap_or_else(|e| e.into_inner()));
        self.db.insert_checks(&rows);
    }

    /// 24 h buckets, cached for 30 s (sparklines and the heatmap share them).
    pub fn buckets(&self) -> Arc<(i64, HashMap<String, [Bucket; BUCKETS]>)> {
        let t = now();
        if let Some((at, b)) = self.buckets_cache.lock().unwrap_or_else(|e| e.into_inner()).as_ref()
            && t - at < 30 {
                return b.clone();
            }
        self.flush_checks();
        let b = Arc::new(self.db.buckets_24h());
        *self.buckets_cache.lock().unwrap_or_else(|e| e.into_inner()) = Some((t, b.clone()));
        b
    }
}

#[derive(Clone, Debug)]
pub enum LiveEvent {
    Check { target: String, ok: bool, ms: Option<f64> },
    Changed { mood: &'static str },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Unknown,
    Up,
    Degraded,
    Down,
    Unmonitored,
}

impl Status {
    pub fn css(self) -> &'static str {
        match self {
            Status::Unknown => "unknown",
            Status::Up => "up",
            Status::Degraded => "degraded",
            Status::Down => "down",
            Status::Unmonitored => "paused",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Status::Unknown => "Checking",
            Status::Up => "Up",
            Status::Degraded => "Slow",
            Status::Down => "Down",
            Status::Unmonitored => "Not monitored",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetKind {
    Router,
    Internet,
    Device(Kind),
}

#[derive(Clone, Debug)]
pub struct Target {
    pub id: String,
    pub name: String,
    pub kind: TargetKind,
    pub host: String,
    pub icon: String,
    pub mac: Option<String>,
    pub tcp_port: Option<u16>,
    pub monitored: bool,
    pub status: Status,
    pub since: i64,
    pub latency: Option<f64>,
    pub last_ok: Option<i64>,
    pub last_check: Option<i64>,
    pub consec_fail: u32,
    pub recent: VecDeque<bool>,
    pub first_fail: Option<i64>,
    pub outage: Option<i64>,
    pub tcp_ok: Option<bool>,
    pub resolved: Option<String>,
    pub down_after: u32,
    pub degraded_ms: f64,
    /// Set on the router target when the server itself has no network.
    pub server_offline: bool,
    /// Whether a "down" alert went into the queue for the current outage.
    pub alerted: bool,
}

pub struct Applied {
    pub prev: Status,
    pub new: Status,
}

impl Target {
    #[allow(clippy::too_many_arguments)]
    pub fn new(id: &str, name: &str, kind: TargetKind, host: String, icon: &str, down_after: u32, degraded_ms: f64, monitored: bool) -> Target {
        Target {
            id: id.into(),
            name: name.into(),
            kind,
            host,
            icon: icon.into(),
            mac: None,
            tcp_port: None,
            monitored,
            status: if monitored { Status::Unknown } else { Status::Unmonitored },
            since: now(),
            latency: None,
            last_ok: None,
            last_check: None,
            consec_fail: 0,
            recent: VecDeque::with_capacity(10),
            first_fail: None,
            outage: None,
            tcp_ok: None,
            resolved: None,
            down_after: down_after.max(1),
            degraded_ms,
            server_offline: false,
            alerted: false,
        }
    }

    fn adopt(&mut self, prev: Target) {
        if !self.monitored {
            return;
        }
        self.status = if prev.status == Status::Unmonitored { Status::Unknown } else { prev.status };
        self.since = prev.since;
        self.latency = prev.latency;
        self.last_ok = prev.last_ok;
        self.last_check = prev.last_check;
        self.consec_fail = prev.consec_fail;
        self.recent = prev.recent;
        self.first_fail = prev.first_fail;
        self.outage = prev.outage;
        self.tcp_ok = prev.tcp_ok;
        self.resolved = prev.resolved;
        self.server_offline = prev.server_offline;
        self.alerted = prev.alerted;
    }

    pub fn is_device(&self) -> bool {
        matches!(self.kind, TargetKind::Device(_))
    }

    pub fn device_kind(&self) -> Option<Kind> {
        match self.kind {
            TargetKind::Device(k) => Some(k),
            _ => None,
        }
    }

    pub fn loss_pct(&self) -> f64 {
        if self.recent.is_empty() {
            return 0.0;
        }
        let fails = self.recent.iter().filter(|ok| !**ok).count();
        fails as f64 * 100.0 / self.recent.len() as f64
    }

    /// Loss only counts once there are enough samples; one miss out of two is not "slow".
    fn lossy(&self, limit: f64) -> bool {
        self.recent.len() >= 5 && self.loss_pct() >= limit
    }

    /// Feed one check result in; returns the status before and after.
    pub fn apply(&mut self, ok: bool, ms: Option<f64>, t: i64, loss_limit: f64) -> Applied {
        let prev = self.status;
        if self.recent.len() == 10 {
            self.recent.pop_front();
        }
        self.recent.push_back(ok);
        self.last_check = Some(t);
        let new = if ok {
            self.consec_fail = 0;
            self.first_fail = None;
            self.last_ok = Some(t);
            self.latency = ms;
            let slow = ms.is_some_and(|m| m > self.degraded_ms);
            if slow || self.lossy(loss_limit) { Status::Degraded } else { Status::Up }
        } else {
            self.consec_fail += 1;
            if self.consec_fail == 1 {
                self.first_fail = Some(t);
            }
            if self.consec_fail >= self.down_after || prev == Status::Down {
                Status::Down
            } else if prev == Status::Unknown {
                Status::Unknown
            } else if self.lossy(loss_limit) {
                Status::Degraded
            } else {
                prev
            }
        };
        if new != prev {
            self.since = if new == Status::Down { self.first_fail.unwrap_or(t) } else { t };
        }
        self.status = new;
        Applied { prev, new }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Ok,
    Warn,
    Bad,
}

impl Tone {
    pub fn css(self) -> &'static str {
        match self {
            Tone::Ok => "ok",
            Tone::Warn => "warn",
            Tone::Bad => "bad",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Overview {
    pub verdict: String,
    pub tone: Tone,
    /// 0 = fully lit orb, 1 = full eclipse.
    pub eclipse: f64,
    pub mood: &'static str,
    pub internet: Status,
    pub router: Status,
    pub devices_up: usize,
    pub devices_total: usize,
    pub devices_down: usize,
    pub devices_slow: usize,
    pub server_offline: bool,
    pub on_battery: bool,
}

pub fn overview(targets: &[Target], on_battery: bool, battery: Option<f64>) -> Overview {
    let find = |id: &str| targets.iter().find(|t| t.id == id);
    let router = find("router").map(|t| t.status).unwrap_or(Status::Unknown);
    let internet = find("internet").map(|t| t.status).unwrap_or(Status::Unknown);
    let server_offline = find("router").is_some_and(|t| t.server_offline);
    let devices: Vec<&Target> = targets.iter().filter(|t| t.is_device() && t.monitored).collect();
    let total = devices.len();
    let down = devices.iter().filter(|t| t.status == Status::Down).count();
    let slow = devices.iter().filter(|t| t.status == Status::Degraded).count();
    let up = devices.iter().filter(|t| matches!(t.status, Status::Up | Status::Degraded)).count();
    let frac = |n: usize| if total == 0 { 0.0 } else { n as f64 / total as f64 };
    let plural = |n: usize, one: &str, many: &str| if n == 1 { format!("1 {one}") } else { format!("{n} {many}") };

    let (verdict, tone, eclipse) = if on_battery {
        let b = battery.map(|b| format!(", server on battery {b:.0}%")).unwrap_or_default();
        (format!("Power outage{b}"), Tone::Bad, 0.8)
    } else if server_offline {
        ("Server offline".to_string(), Tone::Bad, 0.92)
    } else if router == Status::Down {
        ("Router down".to_string(), Tone::Bad, 0.88)
    } else if internet == Status::Down {
        ("Internet down".to_string(), Tone::Bad, 0.7)
    } else if down > 0 {
        (format!("{} down", plural(down, "device", "devices")), Tone::Bad, 0.2 + 0.5 * frac(down))
    } else if internet == Status::Degraded || router == Status::Degraded {
        ("Internet slow".to_string(), Tone::Warn, 0.18)
    } else if slow > 0 {
        (format!("{} slow", plural(slow, "device", "devices")), Tone::Warn, 0.1 + 0.25 * frac(slow))
    } else if router == Status::Unknown && internet == Status::Unknown {
        ("Checking…".to_string(), Tone::Ok, 0.04)
    } else {
        ("All clear".to_string(), Tone::Ok, 0.0)
    };
    let mood = match tone {
        Tone::Ok => "calm",
        Tone::Warn => "warn",
        Tone::Bad => "alert",
    };
    Overview {
        verdict,
        tone,
        eclipse: eclipse.clamp(0.0, 1.0),
        mood,
        internet,
        router,
        devices_up: up,
        devices_total: total,
        devices_down: down,
        devices_slow: slow,
        server_offline,
        on_battery,
    }
}
