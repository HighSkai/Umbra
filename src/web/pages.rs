//! Page handlers and the view models the templates render.

use crate::config::Kind;
use crate::db::{BUCKETS, Bucket};
use crate::integrations::{self, Camera};
use crate::state::{App, Shared, Status, Target, TargetKind};
use crate::util::{esc, fmt_ago, fmt_bytes, fmt_clock, fmt_date, fmt_dur, fmt_ms, fmt_when, now};
use crate::web::assets;
use crate::web::auth::Session;
use crate::web::svg;
use askama::Template;
use axum::Extension;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::Ordering;

/// Templates call `self.icon("name")`.
pub trait Icons {
    fn icon(&self, name: &str) -> String {
        assets::icon(name)
    }
}

pub struct Chrome {
    pub title: String,
    pub site: String,
    pub css: &'static str,
    pub js: &'static str,
    pub live_js: &'static str,
    pub font: &'static str,
    pub favicon: &'static str,
    pub csrf: String,
    pub palette: String,
    pub mood: &'static str,
    pub page: &'static str,
}

impl Chrome {
    pub fn bare(app: &App, title: &str) -> Chrome {
        Chrome {
            title: title.into(),
            site: app.cfg().server.name.clone(),
            css: assets::CSS,
            js: assets::JS,
            live_js: assets::LIVE_JS,
            font: assets::FONT_MONO,
            favicon: assets::FAVICON,
            csrf: String::new(),
            palette: "[]".into(),
            mood: "calm",
            page: "login",
        }
    }

    pub fn full(app: &App, s: &Session, title: &str, page: &'static str) -> Chrome {
        let ov = app.overview();
        Chrome { csrf: s.csrf.clone(), palette: palette(app), mood: ov.mood, page, ..Chrome::bare(app, title) }
    }

    pub fn csrf_header(&self) -> String {
        json!({ "X-CSRF-Token": self.csrf }).to_string()
    }
}

/// Command palette entries (Ctrl+K), embedded in every page.
fn palette(app: &App) -> String {
    let mut items = vec![
        json!({ "label": "Dashboard", "hint": "Go to", "href": "/", "icon": "house" }),
        json!({ "label": "Settings", "hint": "Go to", "href": "/settings", "icon": "settings" }),
        json!({ "label": "Add a device", "hint": "Settings", "href": "/settings#add-device", "icon": "plus" }),
        json!({ "label": "Alerts", "hint": "Settings", "href": "/settings#alerts", "icon": "bell-ring" }),
        json!({ "label": "Debug stats", "hint": "Go to", "href": "/debug/stats", "icon": "activity" }),
    ];
    for t in app.targets().iter() {
        items.push(json!({ "label": t.name, "hint": "Device", "href": format!("/device/{}", t.id), "icon": t.icon }));
        if t.mac.is_some() {
            items.push(json!({ "label": format!("Wake {}", t.name), "hint": "Wake-on-LAN", "post": format!("/device/{}/wake", t.id), "icon": "power" }));
        }
        items.push(json!({ "label": format!("Traceroute to {}", t.name), "hint": "Network", "href": format!("/device/{}?trace=1", t.id), "icon": "route" }));
    }
    let st = app.integ.lock().unwrap_or_else(|e| e.into_inner());
    for c in st.ring.cameras.iter().chain(st.blink.cameras.iter()) {
        items.push(json!({ "label": format!("Live view: {}", c.name), "hint": integrations::title(&c.source), "live": c.source, "device": c.id, "name": c.name, "icon": "video" }));
    }
    crate::util::json_for_script(&serde_json::Value::Array(items))
}

// ---------------------------------------------------------------------------
// Header
// ---------------------------------------------------------------------------

pub struct Chip {
    pub label: &'static str,
    pub value: String,
    pub tone: &'static str,
    pub icon: &'static str,
    pub href: String,
    pub lat: Option<&'static str>,
}

pub struct HeaderVm {
    pub verdict: String,
    pub tone: &'static str,
    pub orb: String,
    pub chips: Vec<Chip>,
}

fn status_tone(s: Status) -> &'static str {
    match s {
        Status::Up => "ok",
        Status::Degraded => "warn",
        Status::Down => "bad",
        _ => "muted",
    }
}

fn target_value(t: Option<&Target>) -> String {
    match t {
        None => "—".into(),
        Some(t) => match t.status {
            Status::Down => format!("Down {}", fmt_dur(now() - t.since)),
            Status::Unknown => "Checking".into(),
            Status::Unmonitored => "Off".into(),
            _ => fmt_ms(t.latency),
        },
    }
}

pub fn header(app: &App) -> HeaderVm {
    let ov = app.overview();
    let targets = app.targets().clone();
    let find = |id: &str| targets.iter().find(|t| t.id == id);
    let host = app.host.lock().map(|h| h.clone()).unwrap_or_default();
    let mut chips = vec![];
    chips.push(Chip { label: "Internet", value: target_value(find("internet")), tone: status_tone(ov.internet), icon: "globe", href: "/device/internet".into(), lat: Some("internet") });
    chips.push(Chip { label: "Router", value: target_value(find("router")), tone: if ov.server_offline { "bad" } else { status_tone(ov.router) }, icon: "router", href: "/device/router".into(), lat: Some("router") });
    let (pv, pt) = match host.on_ac {
        Some(true) => ("On AC".to_string(), "ok"),
        Some(false) => (host.battery_pct.map(|b| format!("Battery {b:.0}%")).unwrap_or_else(|| "On battery".into()), "bad"),
        None => ("Unknown".to_string(), "muted"),
    };
    chips.push(Chip { label: "Power", value: pv, tone: pt, icon: if host.on_ac == Some(false) { "zap-off" } else { "plug-zap" }, href: "/#server".into(), lat: None });
    let (dv, dt) = if ov.devices_total == 0 {
        ("None yet".to_string(), "muted")
    } else {
        (format!("{}/{} up", ov.devices_up, ov.devices_total), if ov.devices_down > 0 { "bad" } else if ov.devices_slow > 0 { "warn" } else { "ok" })
    };
    chips.push(Chip { label: "Devices", value: dv, tone: dt, icon: "monitor", href: if ov.devices_total == 0 { "/settings#add-device".into() } else { "/#devices".into() }, lat: None });
    chips.push(Chip { label: "Thermostat", value: "Not set up".into(), tone: "muted", icon: "thermometer", href: "/settings#thermostat".into(), lat: None });
    chips.push(camera_chip(app));
    HeaderVm { verdict: ov.verdict, tone: ov.tone.css(), orb: svg::orb(ov.eclipse, ov.tone.css()), chips }
}

fn camera_chip(app: &App) -> Chip {
    let st = app.integ.lock().unwrap_or_else(|e| e.into_inner());
    for name in integrations::SOURCES {
        if st.get(name).needs_login() {
            return Chip { label: "Cameras", value: format!("{}: reconnect", integrations::title(name)), tone: "warn", icon: "cctv", href: format!("/settings#integ-{name}"), lat: None };
        }
    }
    drop(st);
    let any = integrations::SOURCES.iter().any(|n| integrations::connected(app, n));
    if !any {
        return Chip { label: "Cameras", value: "Not connected".into(), tone: "muted", icon: "cctv", href: "/settings#cameras".into(), lat: None };
    }
    match app.db.cam_events_since(now() - 86_400).first() {
        Some(e) => Chip {
            label: "Cameras",
            value: format!("{} {}", if e.kind == "ding" { "Ding" } else { "Motion" }, fmt_when(e.ts)),
            tone: "cam",
            icon: if e.kind == "ding" { "bell-ring" } else { "cctv" },
            href: "/#front-door".into(),
            lat: None,
        },
        None => Chip { label: "Cameras", value: "No events today".into(), tone: "muted", icon: "cctv", href: "/#front-door".into(), lat: None },
    }
}

// ---------------------------------------------------------------------------
// Dashboard body
// ---------------------------------------------------------------------------

pub struct CardVm {
    pub id: String,
    pub name: String,
    pub icon: String,
    pub status: &'static str,
    pub status_label: &'static str,
    pub metric: String,
    pub sub: String,
    pub spark: String,
}

pub struct GroupVm {
    pub title: &'static str,
    pub cards: Vec<CardVm>,
}

pub struct HeatRow {
    pub name: String,
    pub href: String,
    pub strip: String,
    pub uptime: String,
}

pub struct CamVm {
    pub source: String,
    pub id: String,
    pub name: String,
    pub model: String,
    pub online: &'static str,
    pub online_label: &'static str,
    pub power: String,
    pub wifi: String,
    pub armed: String,
    pub last_ding: String,
    pub last_motion: String,
    pub doorbell: bool,
    pub can_live: bool,
    pub can_snapshot: bool,
    pub snapshot_url: Option<String>,
    pub snapshot_when: String,
}

pub struct DoorSource {
    pub name: &'static str,
    pub title: &'static str,
    pub connected: bool,
    pub banner: Option<String>,
    pub banner_tone: &'static str,
    pub checked: String,
    pub cameras: Vec<CamVm>,
}

pub struct TlEvent {
    pub left: String,
    pub kind: String,
    pub title: String,
}

pub struct DoorVm {
    pub sources: Vec<DoorSource>,
    pub events: Vec<TlEvent>,
    pub hours: Vec<(String, String)>,
    pub any_connected: bool,
}

pub struct ServerVm {
    pub hostname: String,
    pub os: String,
    pub power: String,
    pub power_tone: &'static str,
    pub battery: String,
    pub health: String,
    pub temp: String,
    pub disk: String,
    pub uptime: String,
}

pub struct LiveVm {
    pub map: String,
    pub path_summary: Vec<(String, &'static str)>,
    pub groups: Vec<GroupVm>,
    pub heat: Vec<HeatRow>,
    pub heat_hours: Vec<(String, String)>,
    pub heat_defs: &'static str,
    pub door: DoorVm,
    pub server: ServerVm,
    pub alerts_ready: bool,
}

fn empty_buckets() -> [Bucket; BUCKETS] {
    [Bucket::default(); BUCKETS]
}

fn card_metric(t: &Target) -> String {
    match t.status {
        Status::Up | Status::Degraded => fmt_ms(t.latency),
        Status::Down => format!("Down {}", fmt_dur(now() - t.since)),
        Status::Unknown => "Checking".into(),
        Status::Unmonitored => "Not monitored".into(),
    }
}

fn card_sub(t: &Target) -> String {
    match (t.status, t.last_ok) {
        (Status::Down, Some(ok)) => format!("{} · last seen {}", t.host, fmt_ago(ok)),
        (Status::Down, None) => format!("{} · never seen", t.host),
        _ => t.host.clone(),
    }
}

fn uptime_text(b: &[Bucket; BUCKETS]) -> String {
    let (n, ok) = b.iter().fold((0u32, 0u32), |(n, ok), x| (n + x.n, ok + x.ok));
    if n == 0 {
        return "—".into();
    }
    let pct = f64::from(ok) * 100.0 / f64::from(n);
    if pct >= 99.95 { "100%".into() } else { format!("{pct:.1}%") }
}

fn hour_marks(start: i64, span_buckets: usize) -> Vec<(String, String)> {
    [0usize, 24, 48, 72, span_buckets - 1]
        .iter()
        .map(|i| (format!("{:.2}%", *i as f64 * 100.0 / span_buckets as f64), if *i == span_buckets - 1 { "now".to_string() } else { fmt_clock(start + *i as i64 * 900) }))
        .collect()
}

pub fn live_vm(app: &App) -> LiveVm {
    let targets = app.targets().clone();
    let buckets = app.buckets();
    let (start, map_b) = (&buckets.0, &buckets.1);
    let get_b = |id: &str| map_b.get(id).copied().unwrap_or_else(empty_buckets);

    let to_node = |t: &Target| svg::MapNode {
        id: t.id.clone(),
        name: t.name.clone(),
        icon: t.icon.clone(),
        status: t.status.css(),
        latency: if t.status == Status::Down { "down".into() } else { fmt_ms(t.latency) },
        href: format!("/device/{}", t.id),
    };
    let internet = targets.iter().find(|t| t.id == "internet").map(to_node).unwrap_or_else(|| svg::MapNode { id: "internet".into(), name: "Internet".into(), icon: "globe".into(), status: "unknown", latency: "—".into(), href: "#".into() });
    let router = targets.iter().find(|t| t.id == "router").map(to_node).unwrap_or_else(|| svg::MapNode { id: "router".into(), name: "Router".into(), icon: "router".into(), status: "unknown", latency: "—".into(), href: "#".into() });

    let mut devs: Vec<&Target> = targets.iter().filter(|t| t.is_device()).collect();
    devs.sort_by_key(|t| t.device_kind().map(|k| k.group().0).unwrap_or(9));
    let nodes: Vec<svg::MapNode> = devs.iter().map(|t| to_node(t)).collect();
    let map = svg::map(&internet, &router, &nodes);

    let mut groups: Vec<GroupVm> = vec![];
    for t in &devs {
        let k = t.device_kind().unwrap_or(Kind::Other);
        let (_, title) = k.group();
        let card = CardVm {
            id: t.id.clone(),
            name: t.name.clone(),
            icon: assets::icon(&t.icon),
            status: t.status.css(),
            status_label: t.status.label(),
            metric: card_metric(t),
            sub: card_sub(t),
            spark: svg::sparkline(&get_b(&t.id), 24.0, ""),
        };
        match groups.iter_mut().find(|g| g.title == title) {
            Some(g) => g.cards.push(card),
            None => groups.push(GroupVm { title, cards: vec![card] }),
        }
    }

    let mut heat = vec![];
    for t in targets.iter().filter(|t| t.monitored) {
        let b = get_b(&t.id);
        heat.push(HeatRow { name: t.name.clone(), href: format!("/device/{}", t.id), strip: svg::heat_strip(&b), uptime: uptime_text(&b) });
    }

    let ov = crate::state::overview(&targets, false, None);
    let path_summary = vec![
        ("Internet".to_string(), ov.internet.css()),
        ("Router".to_string(), ov.router.css()),
        (if ov.devices_total == 0 { "No devices".to_string() } else { format!("{}/{} devices up", ov.devices_up, ov.devices_total) }, if ov.devices_down > 0 { "down" } else if ov.devices_slow > 0 { "degraded" } else { "up" }),
    ];

    LiveVm {
        map,
        path_summary,
        groups,
        heat,
        heat_hours: hour_marks(*start, BUCKETS),
        heat_defs: svg::HEAT_DEFS,
        door: door_vm(app),
        server: server_vm(app),
        alerts_ready: crate::alerts::configured(app),
    }
}

fn cam_vm(c: &Camera, connected: bool) -> CamVm {
    let (online, online_label) = match c.online {
        Some(true) => ("up", "Online"),
        Some(false) => ("down", "Offline"),
        None => ("unknown", "Unknown"),
    };
    let power = match (c.wired, c.battery) {
        (Some(true), Some(b)) => format!("Wired · battery {b:.0}%"),
        (Some(true), None) => "Wired".into(),
        (_, Some(b)) => format!("Battery {b:.0}%"),
        _ => String::new(),
    };
    let wifi = match (c.wifi_rssi, c.wifi_bars) {
        (Some(r), _) => format!("{r} dBm · {}", if r >= -60 { "strong" } else if r >= -70 { "fair" } else { "weak" }),
        (None, Some(b)) => format!("{b}/5 bars"),
        _ => String::new(),
    };
    CamVm {
        source: c.source.clone(),
        id: c.id.clone(),
        name: c.name.clone(),
        model: c.model.clone(),
        online,
        online_label,
        power,
        wifi,
        armed: match c.armed {
            Some(true) => "Armed".into(),
            Some(false) => "Disarmed".into(),
            None => String::new(),
        },
        last_ding: c.last_ding.map(fmt_when).unwrap_or_else(|| "—".into()),
        last_motion: c.last_motion.map(fmt_when).unwrap_or_else(|| "—".into()),
        doorbell: c.doorbell,
        can_live: connected && c.online != Some(false),
        can_snapshot: connected && c.source == "blink",
        snapshot_url: c.snapshot_at.map(|t| format!("/cameras/{}/{}/snapshot.jpg?t={t}", c.source, c.id)),
        snapshot_when: c.snapshot_at.map(fmt_when).unwrap_or_default(),
    }
}

fn door_vm(app: &App) -> DoorVm {
    let st = app.integ.lock().map(|s| (s.ring.clone(), s.blink.clone())).unwrap_or_default();
    let mut sources = vec![];
    let mut names: HashMap<String, String> = HashMap::new();
    for (name, src) in [("ring", &st.0), ("blink", &st.1)] {
        let connected = integrations::connected(app, name);
        let title = integrations::title(name);
        let (banner, tone) = if src.needs_login() {
            (Some(format!("{title}: login expired, reconnect")), "bad")
        } else if let Some(msg) = &src.message {
            let worked = src.last_ok.map(|t| format!(" Last worked {}.", fmt_ago(t))).unwrap_or_default();
            (Some(format!("{title}: {msg}.{worked}")), "warn")
        } else {
            (None, "")
        };
        for c in &src.cameras {
            names.insert(format!("{}:{}", c.source, c.id), c.name.clone());
        }
        sources.push(DoorSource {
            name,
            title,
            connected,
            banner,
            banner_tone: tone,
            checked: src.last_ok.map(|t| format!("checked {}", fmt_ago(t))).unwrap_or_default(),
            cameras: src.cameras.iter().map(|c| cam_vm(c, connected)).collect(),
        });
    }
    let since = now() - 86_400;
    let events = app
        .db
        .cam_events_since(since)
        .into_iter()
        .map(|e| {
            let who = names.get(&format!("{}:{}", e.source, e.device)).cloned().unwrap_or_else(|| integrations::title(&e.source).to_string());
            TlEvent {
                left: format!("{:.2}%", (e.ts - since) as f64 * 100.0 / 86_400.0),
                kind: e.kind.clone(),
                title: format!("{} · {} · {}", if e.kind == "ding" { "Ding" } else { "Motion" }, who, fmt_clock(e.ts)),
            }
        })
        .collect();
    let any_connected = sources.iter().any(|s| s.connected);
    let start = (now() / 900) * 900 - 95 * 900;
    DoorVm { sources, events, hours: hour_marks(start, BUCKETS), any_connected }
}

fn server_vm(app: &App) -> ServerVm {
    let h = app.host.lock().map(|h| h.clone()).unwrap_or_default();
    let x = if app.demo {
        // Don't show anything about the machine the demo happens to run on.
        crate::host::HostExtra {
            hostname: "demo-server".into(),
            os: "Linux".into(),
            cpu_temp_c: Some(46.0),
            disk_free: Some(88_400_000_000),
            disk_total: Some(237_900_000_000),
            uptime_secs: Some(12 * 86_400 + 5 * 3600 + (now() - app.started_unix) as u64),
        }
    } else {
        crate::host::extra(&app.cfg().server.data_dir)
    };
    let (power, power_tone) = match h.on_ac {
        Some(true) => ("On AC power".to_string(), "ok"),
        Some(false) => (format!("On battery since {}", h.since.map(fmt_clock).unwrap_or_default()), "bad"),
        None => ("Unknown".to_string(), "muted"),
    };
    ServerVm {
        hostname: if app.demo { "demo-server".into() } else if x.hostname.is_empty() { "This server".into() } else { x.hostname },
        os: x.os,
        power,
        power_tone,
        battery: match (h.has_battery, h.battery_pct) {
            (true, Some(p)) => format!("{p:.0}%{}", h.battery_state.as_ref().map(|s| format!(" · {}", s.to_lowercase())).unwrap_or_default()),
            (true, None) => "Present".into(),
            _ => "No battery".into(),
        },
        health: match (h.health_pct(), h.full_wh, h.design_wh) {
            (Some(p), Some(f), Some(d)) => format!("{p:.0}% of design ({f:.1} of {d:.1} Wh)"),
            _ => "Not reported".into(),
        },
        temp: x.cpu_temp_c.map(|t| format!("{t:.0} °C")).unwrap_or_else(|| "Not available".into()),
        disk: match (x.disk_free, x.disk_total) {
            (Some(f), Some(t)) => format!("{} free of {}", fmt_bytes(f), fmt_bytes(t)),
            _ => "—".into(),
        },
        uptime: x.uptime_secs.map(|u| fmt_dur(u as i64)).unwrap_or_else(|| "—".into()),
    }
}

// ---------------------------------------------------------------------------
// Templates and handlers
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "dashboard.html")]
pub struct DashboardPage {
    pub chrome: Chrome,
    pub header: HeaderVm,
    pub live: LiveVm,
}
impl Icons for DashboardPage {}

#[derive(Template)]
#[template(path = "partials/header.html")]
pub struct HeaderPartial {
    pub header: HeaderVm,
}
impl Icons for HeaderPartial {}

#[derive(Template)]
#[template(path = "partials/live.html")]
pub struct LivePartial {
    pub live: LiveVm,
}
impl Icons for LivePartial {}

fn render(t: &impl Template) -> Response {
    match t.render() {
        Ok(s) => Html(s).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("template error: {e}")).into_response(),
    }
}

pub async fn dashboard(State(app): State<Shared>, Extension(s): Extension<Session>) -> Response {
    let header = header(&app);
    let chrome = Chrome::full(&app, &s, &header.verdict, "home");
    render(&DashboardPage { chrome, header, live: live_vm(&app) })
}

pub async fn header_partial(State(app): State<Shared>) -> Response {
    render(&HeaderPartial { header: header(&app) })
}

pub async fn live_partial(State(app): State<Shared>) -> Response {
    render(&LivePartial { live: live_vm(&app) })
}

pub struct DayVm {
    pub label: String,
    pub pct: String,
    pub cls: &'static str,
    pub height: String,
}

pub struct OutageVm {
    pub start: String,
    pub end: String,
    pub dur: String,
}

pub struct DeviceVm {
    pub id: String,
    pub name: String,
    pub icon: String,
    pub kind_label: String,
    pub host: String,
    pub resolved: String,
    pub mac: String,
    pub status: &'static str,
    pub status_label: &'static str,
    pub since: String,
    pub latency: String,
    pub last_seen: String,
    pub loss: String,
    pub tcp: String,
    pub interval: String,
    pub chart: String,
    pub chart_hours: Vec<(String, String)>,
    pub chart_range: String,
    pub days: Vec<DayVm>,
    pub uptime_90: String,
    pub outages: Vec<OutageVm>,
    pub can_wake: bool,
    pub editable: bool,
    pub auto_trace: bool,
}

#[derive(Template)]
#[template(path = "device.html")]
pub struct DevicePage {
    pub chrome: Chrome,
    pub header: HeaderVm,
    pub d: DeviceVm,
}
impl Icons for DevicePage {}

pub async fn device_page(State(app): State<Shared>, Extension(s): Extension<Session>, Path(id): Path<String>, Query(q): Query<HashMap<String, String>>) -> Response {
    let Some(t) = app.targets().iter().find(|t| t.id == id).cloned() else {
        return (StatusCode::NOT_FOUND, Html("No such device. <a href=\"/\">Back</a>")).into_response();
    };
    let b = app.buckets().1.get(&id).copied().unwrap_or_else(empty_buckets);
    let start = app.buckets().0;
    let lats: Vec<f64> = b.iter().filter_map(|x| x.lat).collect();
    let chart_range = if lats.is_empty() {
        "No data yet".into()
    } else {
        let lo = lats.iter().cloned().fold(f64::INFINITY, f64::min);
        let hi = lats.iter().cloned().fold(0.0, f64::max);
        format!("{} – {}", fmt_ms(Some(lo)), fmt_ms(Some(hi)))
    };
    let days = app
        .db
        .daily_uptime(&id, 7)
        .into_iter()
        .map(|(day, n, ok)| {
            let p = if n == 0 { 0.0 } else { f64::from(ok) / f64::from(n) };
            DayVm {
                label: fmt_date(day + 43_200),
                pct: if p >= 0.9995 { "100%".into() } else { format!("{:.1}%", p * 100.0) },
                cls: if p >= 0.995 { "up" } else if p >= 0.9 { "degraded" } else { "down" },
                height: format!("{:.0}%", (p * 100.0).max(4.0)),
            }
        })
        .collect();
    let outages = app
        .db
        .outages(Some(&id), 25)
        .into_iter()
        .map(|o| OutageVm { start: fmt_when(o.started), end: o.ended.map(fmt_when).unwrap_or_else(|| "ongoing".into()), dur: fmt_dur(o.ended.unwrap_or_else(now) - o.started) })
        .collect();
    let cfg_dev = app.cfg().device(&id).cloned();
    let kind_label = match t.kind {
        TargetKind::Router => "Router".to_string(),
        TargetKind::Internet => "Internet check".to_string(),
        TargetKind::Device(k) => k.label().to_string(),
    };
    let interval = match t.kind {
        TargetKind::Router => app.cfg().network.router_interval,
        TargetKind::Internet => app.cfg().network.internet_interval,
        TargetKind::Device(_) => cfg_dev.as_ref().map(|d| d.interval()).unwrap_or(60),
    };
    let d = DeviceVm {
        id: t.id.clone(),
        name: t.name.clone(),
        icon: assets::icon(&t.icon),
        kind_label,
        host: t.host.clone(),
        resolved: t.resolved.clone().filter(|r| *r != t.host).unwrap_or_default(),
        mac: t.mac.clone().unwrap_or_default(),
        status: t.status.css(),
        status_label: t.status.label(),
        since: format!("since {}", fmt_when(t.since)),
        latency: fmt_ms(t.latency),
        last_seen: t.last_ok.map(fmt_ago).unwrap_or_else(|| "never".into()),
        loss: format!("{:.0}%", t.loss_pct()),
        tcp: match (t.tcp_port, t.tcp_ok) {
            (Some(p), Some(true)) => format!("Port {p} open"),
            (Some(p), Some(false)) => format!("Port {p} closed"),
            (Some(p), None) => format!("Port {p} not checked yet"),
            _ => String::new(),
        },
        interval: format!("every {}", fmt_dur(interval as i64)),
        chart: svg::sparkline(&b, 60.0, "spark-lg"),
        chart_hours: hour_marks(start, BUCKETS),
        chart_range,
        days,
        uptime_90: app.db.uptime_90d(&id).map(|u| format!("{:.2}%", u * 100.0)).unwrap_or_else(|| "—".into()),
        outages,
        can_wake: t.mac.is_some(),
        editable: t.is_device(),
        auto_trace: q.get("trace").is_some_and(|v| v == "1"),
    };
    let header = header(&app);
    let chrome = Chrome::full(&app, &s, &t.name, "device");
    render(&DevicePage { chrome, header, d })
}

pub fn toast(tone: &str, msg: &str) -> Response {
    let icon = match tone {
        "ok" => "check",
        "bad" => "triangle-alert",
        _ => "circle-help",
    };
    Html(format!(r#"<div class="toast toast-{tone}" role="status">{}<span>{}</span></div>"#, assets::icon(icon), esc(msg))).into_response()
}

pub async fn wake(State(app): State<Shared>, Path(id): Path<String>) -> Response {
    if app.demo {
        return toast("ok", "Demo: a real install would send the wake-up packet now.");
    }
    let (name, mac) = match app.targets().iter().find(|t| t.id == id) {
        Some(t) => (t.name.clone(), t.mac.clone()),
        None => return toast("bad", "Unknown device"),
    };
    let Some(mac) = mac.as_deref().and_then(crate::util::parse_mac) else {
        return toast("bad", &format!("{name} has no MAC address. Add one in Settings."));
    };
    match crate::net::wake_on_lan(mac) {
        Ok(()) => toast("ok", &format!("Wake-up packet sent to {name}. It can take a minute to come online.")),
        Err(e) => toast("bad", &format!("Couldn't send the wake-up packet: {e}")),
    }
}

pub async fn traceroute(State(app): State<Shared>, Path(id): Path<String>) -> Response {
    if app.demo {
        let sample = "traceroute to 1.1.1.1, 20 hops max\n 1  192.168.1.1   1.8 ms\n 2  10.42.0.1     6.9 ms\n 3  203.0.113.9  11.2 ms\n 4  1.1.1.1      13.4 ms\n\n(demo: simulated output)";
        return Html(format!("<pre class=\"trace mono\">{}</pre>", esc(sample))).into_response();
    }
    let host = match app.targets().iter().find(|t| t.id == id) {
        Some(t) if t.id == "internet" => app.cfg().network.internet_targets.first().cloned().unwrap_or_default(),
        Some(t) if t.id == "router" => t.resolved.clone().unwrap_or_else(|| t.host.clone()),
        Some(t) => t.host.clone(),
        None => return Html("<pre class=\"trace\">Unknown device</pre>".to_string()).into_response(),
    };
    if app.traceroute_busy.swap(true, Ordering::SeqCst) {
        return Html("<pre class=\"trace\">Another traceroute is running. Try again in a moment.</pre>".to_string()).into_response();
    }
    let out = crate::net::traceroute(&host).await;
    app.traceroute_busy.store(false, Ordering::SeqCst);
    Html(format!("<pre class=\"trace mono\">{}</pre>", esc(&out))).into_response()
}

pub async fn blink_snapshot(State(app): State<Shared>, Path(id): Path<String>) -> Response {
    match integrations::snapshot(&app, &id).await {
        Ok(()) => toast("ok", "Snapshot taken."),
        Err(e) => toast("bad", &e),
    }
}

pub async fn snapshot_image(State(app): State<Shared>, Path((source, id)): Path<(String, String)>) -> Response {
    if source != "blink" {
        return StatusCode::NOT_FOUND.into_response();
    }
    match std::fs::read(integrations::snapshot_path(&app, &source, &id)) {
        Ok(bytes) => {
            let mut r = bytes.into_response();
            r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("image/jpeg"));
            r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("private, max-age=300"));
            r
        }
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

// ---------------------------------------------------------------------------
// /debug/stats
// ---------------------------------------------------------------------------

pub struct HelperVm {
    pub what: String,
    pub when: String,
    pub ms: String,
    pub ok: bool,
    pub note: String,
}

pub struct StatsVm {
    pub uptime: String,
    pub cpu: String,
    pub cpu_ok: bool,
    pub cpu_window: String,
    pub cpu_avg: String,
    pub rss: String,
    pub rss_ok: bool,
    pub private: String,
    pub threads: String,
    pub checks_per_min: String,
    pub checks_total: u64,
    pub targets: usize,
    pub sse: usize,
    pub requests: u64,
    pub db_size: String,
    pub rows: Vec<(String, i64)>,
    pub helpers: Vec<HelperVm>,
    pub live: Vec<String>,
    pub pending_alerts: i64,
    pub secrets: String,
    pub platform: String,
}

#[derive(Template)]
#[template(path = "stats.html")]
pub struct StatsPage {
    pub chrome: Chrome,
    pub header: HeaderVm,
    pub st: StatsVm,
}
impl Icons for StatsPage {}

struct Measured {
    cpu_window: Option<(f64, u64)>,
    cpu_avg: Option<f64>,
    mem: crate::stats::Mem,
    uptime: u64,
}

fn measure(app: &App) -> Measured {
    let uptime = app.started.elapsed().as_secs();
    let cpu_avg = crate::stats::process_cpu_secs().map(|c| c / (uptime.max(1) as f64) * 100.0);
    Measured { cpu_window: app.stats.cpu_pct_window(), cpu_avg, mem: crate::stats::memory(), uptime }
}

pub async fn stats_page(State(app): State<Shared>, Extension(s): Extension<Session>) -> Response {
    let m = measure(&app);
    let helpers = app
        .stats
        .helpers
        .lock()
        .map(|q| q.iter().map(|h| HelperVm { what: format!("{} {}", h.name, h.cmd), when: fmt_when(h.at), ms: format!("{:.1} s", h.ms as f64 / 1000.0), ok: h.ok, note: h.note.clone() }).collect())
        .unwrap_or_default();
    let live = crate::live::sessions(&app).await.into_iter().map(|(s, c, secs)| format!("{s} camera {c}, {}", fmt_dur(secs as i64))).collect();
    let st = StatsVm {
        uptime: fmt_dur(m.uptime as i64),
        cpu: m.cpu_window.map(|(p, _)| format!("{p:.2}%")).unwrap_or_else(|| "measuring…".into()),
        cpu_ok: m.cpu_window.is_none_or(|(p, _)| p < 0.5),
        cpu_window: m.cpu_window.map(|(_, w)| format!("over the last {}", fmt_dur(w as i64))).unwrap_or_else(|| "first sample after a minute".into()),
        cpu_avg: m.cpu_avg.map(|p| format!("{p:.2}%")).unwrap_or_else(|| "—".into()),
        rss: m.mem.rss.map(fmt_bytes).unwrap_or_else(|| "—".into()),
        rss_ok: m.mem.rss.is_none_or(|r| r < 20_000_000),
        private: m.mem.private.map(fmt_bytes).unwrap_or_else(|| "—".into()),
        threads: m.mem.threads.map(|t| t.to_string()).unwrap_or_else(|| "—".into()),
        checks_per_min: format!("{:.1}", app.stats.checks_per_min()),
        checks_total: app.stats.checks_total.load(Ordering::Relaxed),
        targets: app.targets().len(),
        sse: app.stats.sse_clients.load(Ordering::Relaxed),
        requests: app.stats.requests.load(Ordering::Relaxed),
        db_size: fmt_bytes(app.db.size_bytes()),
        rows: app.db.row_counts().into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        helpers,
        live,
        pending_alerts: app.db.pending_alerts(),
        secrets: app.secrets.description.clone(),
        platform: format!("{} {} · umbra {}", std::env::consts::OS, std::env::consts::ARCH, env!("CARGO_PKG_VERSION")),
    };
    let header = header(&app);
    let chrome = Chrome::full(&app, &s, "Debug stats", "stats");
    render(&StatsPage { chrome, header, st })
}

pub async fn stats_json(State(app): State<Shared>) -> Response {
    let m = measure(&app);
    let helpers: Vec<_> = app.stats.helpers.lock().map(|q| q.iter().map(|h| json!({ "name": h.name, "cmd": h.cmd, "at": h.at, "ms": h.ms, "ok": h.ok })).collect()).unwrap_or_default();
    axum::Json(json!({
        "uptime_secs": m.uptime,
        "cpu_pct_window": m.cpu_window.map(|w| w.0),
        "cpu_window_secs": m.cpu_window.map(|w| w.1),
        "cpu_pct_since_start": m.cpu_avg,
        "rss_bytes": m.mem.rss,
        "private_bytes": m.mem.private,
        "threads": m.mem.threads,
        "checks_per_min": app.stats.checks_per_min(),
        "checks_total": app.stats.checks_total.load(Ordering::Relaxed),
        "sse_clients": app.stats.sse_clients.load(Ordering::Relaxed),
        "db_bytes": app.db.size_bytes(),
        "pending_alerts": app.db.pending_alerts(),
        "helpers": helpers,
    }))
    .into_response()
}
