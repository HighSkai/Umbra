//! Settings: devices, network, alerts, camera integrations, password.
//! Every change is written back to umbra.toml; secrets go to the keyring.

use crate::config::{Config, Device, KINDS, Kind};
use crate::integrations::{self, LoginOutcome};
use crate::state::Shared;
use crate::util::{esc, fmt_ago, parse_mac};
use crate::web::assets;
use crate::web::auth::Session;
use crate::web::pages::{Chrome, HeaderVm, Icons, header, toast};
use askama::Template;
use axum::Extension;
use axum::extract::{Form, Path, Query, State};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use std::collections::HashMap;

type FormMap = HashMap<String, String>;

pub struct DeviceRow {
    pub id: String,
    pub name: String,
    pub icon: String,
    pub kind_label: &'static str,
    pub host: String,
    pub mac: String,
    pub interval: String,
}

#[derive(Default)]
pub struct DeviceForm {
    pub original_id: String,
    pub name: String,
    pub kind: String,
    pub host: String,
    pub mac: String,
    pub icon: String,
    pub interval: String,
    pub tcp_port: String,
    pub ping: bool,
}

pub struct IntegVm {
    pub name: &'static str,
    pub title: &'static str,
    pub connected: bool,
    pub enabled: bool,
    pub status: String,
    pub problem: Option<String>,
    pub cameras: usize,
}

pub struct SettingsVm {
    pub devices: Vec<DeviceRow>,
    pub kinds: Vec<(&'static str, &'static str)>,
    pub icons: Vec<&'static str>,
    pub form: DeviceForm,
    pub editing: bool,
    pub router: String,
    pub router_interval: u64,
    pub internet_targets: String,
    pub internet_interval: u64,
    pub down_after: u32,
    pub degraded_ms: f64,
    pub gateway: String,
    pub ntfy: bool,
    pub ntfy_server: String,
    pub ntfy_set: bool,
    pub discord: bool,
    pub discord_set: bool,
    pub on_devices: bool,
    pub on_internet: bool,
    pub on_power: bool,
    pub integ: Vec<IntegVm>,
    pub live_minutes: u64,
    pub saved: String,
    pub secrets: String,
    pub python: String,
    pub python_ok: bool,
    pub data_dir: String,
    pub config_path: String,
}

#[derive(Template)]
#[template(path = "settings.html")]
pub struct SettingsPage {
    pub chrome: Chrome,
    pub header: HeaderVm,
    pub s: SettingsVm,
}
impl Icons for SettingsPage {}

impl SettingsPage {
    pub fn login_form(&self, name: &str) -> String {
        login_form(name, "")
    }
    pub fn selected(&self, a: &str, b: &str) -> &'static str {
        if a == b { "selected" } else { "" }
    }
}

fn login_form(name: &str, error: &str) -> String {
    let title = integrations::title(name);
    let err = if error.is_empty() { String::new() } else { format!(r#"<p class="form-error" role="alert">{}</p>"#, esc(error)) };
    let note = if name == "ring" {
        "Ring will text or email you a code. Your password is used once and never stored; Umbra keeps only the refresh token."
    } else {
        "Blink will text or email you a PIN. Your password is used once and never stored; Umbra keeps only the refresh token."
    };
    format!(
        r##"{err}<form class="stack" hx-post="/settings/integration/{name}/login" hx-target="#integ-{name}-flow" hx-swap="innerHTML" hx-disabled-elt="find button">
<p class="muted small">{note}</p>
<div class="row-fields"><label class="field"><span>{title} email</span><input name="username" type="email" autocomplete="off" required></label>
<label class="field"><span>Password</span><input name="password" type="password" autocomplete="off" required></label></div>
<div><button class="btn btn-primary" type="submit">{}<span>Connect {title}</span></button></div></form>"##,
        assets::icon("lock")
    )
}

fn code_form(name: &str, hint: &str) -> String {
    format!(
        r##"<form class="stack" hx-post="/settings/integration/{name}/code" hx-target="#integ-{name}-flow" hx-swap="innerHTML" hx-disabled-elt="find button">
<p class="muted small">{}</p>
<div class="row-fields"><label class="field"><span>Verification code</span><input name="code" inputmode="numeric" autocomplete="one-time-code" maxlength="12" required autofocus></label></div>
<div><button class="btn btn-primary" type="submit">{}<span>Verify</span></button></div></form>"##,
        esc(hint),
        assets::icon("check")
    )
}

fn hx_redirect(to: &str) -> Response {
    let mut r = StatusCode::NO_CONTENT.into_response();
    if let Ok(v) = HeaderValue::from_str(to) {
        r.headers_mut().insert("hx-redirect", v);
    }
    r
}

fn device_form(d: Option<&Device>) -> DeviceForm {
    match d {
        None => DeviceForm { kind: "computer".into(), ping: true, ..Default::default() },
        Some(d) => DeviceForm {
            original_id: d.id.clone(),
            name: d.name.clone(),
            kind: d.kind.key().into(),
            host: d.host.clone(),
            mac: d.mac.clone().unwrap_or_default(),
            icon: d.icon.clone().unwrap_or_default(),
            interval: d.interval.map(|i| i.to_string()).unwrap_or_default(),
            tcp_port: d.tcp_port.map(|p| p.to_string()).unwrap_or_default(),
            ping: d.ping,
        },
    }
}

pub async fn page(State(app): State<Shared>, Extension(sess): Extension<Session>, Query(q): Query<HashMap<String, String>>) -> Response {
    let cfg = app.cfg().clone();
    let edit = q.get("edit").and_then(|id| cfg.device(id));
    let saved = match q.get("saved").map(String::as_str) {
        Some("device") => "Device saved. Checks restarted.",
        Some("deleted") => "Device removed.",
        Some("network") => "Network settings saved.",
        Some("alerts") => "Alert settings saved.",
        Some("ring") => "Ring is connected. The first check runs now.",
        Some("blink") => "Blink is connected. The first check runs now.",
        Some("disconnected") => "Disconnected. The saved token was deleted.",
        _ => "",
    };
    let python = cfg.server.python_path();
    let st = app.integ.lock().map(|s| (s.ring.clone(), s.blink.clone())).unwrap_or_default();
    let integ = [("ring", &st.0, cfg.ring.enabled, cfg.ring_interval()), ("blink", &st.1, cfg.blink.enabled, cfg.blink_interval())]
        .into_iter()
        .map(|(name, src, enabled, interval)| {
            let connected = integrations::connected(&app, name);
            let status = if !connected {
                "Not connected".to_string()
            } else {
                let last = src.last_ok.map(|t| format!("last check {}", fmt_ago(t))).unwrap_or_else(|| "waiting for the first check".into());
                format!("Connected · {last} · every {} min", interval / 60)
            };
            let problem = if src.needs_login() {
                Some("Login expired. Sign in again below.".to_string())
            } else {
                src.message.clone()
            };
            IntegVm { name, title: integrations::title(name), connected, enabled, status, problem, cameras: src.cameras.len() }
        })
        .collect();
    let gw = if app.demo {
        "192.168.1.1 (demo)".to_string()
    } else {
        app.gateway.lock().ok().and_then(|g| *g).map(|g| g.to_string()).unwrap_or_else(|| "not detected".into())
    };
    let s = SettingsVm {
        devices: cfg
            .devices
            .iter()
            .map(|d| DeviceRow {
                id: d.id.clone(),
                name: d.name.clone(),
                icon: assets::icon(d.icon()),
                kind_label: d.kind.label(),
                host: d.host.clone(),
                mac: d.mac.clone().unwrap_or_default(),
                interval: format!("{} s", d.interval()),
            })
            .collect(),
        kinds: KINDS.iter().map(|k| (k.key(), k.label())).collect(),
        icons: assets::ICON_NAMES.to_vec(),
        form: device_form(edit),
        editing: edit.is_some(),
        router: cfg.network.router.clone(),
        router_interval: cfg.network.router_interval,
        internet_targets: cfg.network.internet_targets.join(", "),
        internet_interval: cfg.network.internet_interval,
        down_after: cfg.network.down_after,
        degraded_ms: cfg.network.degraded_ms,
        gateway: gw,
        ntfy: cfg.alerts.ntfy,
        ntfy_server: cfg.alerts.ntfy_server.clone(),
        ntfy_set: app.secrets.has("ntfy_topic"),
        discord: cfg.alerts.discord,
        discord_set: app.secrets.has("discord_webhook"),
        on_devices: cfg.alerts.on_devices,
        on_internet: cfg.alerts.on_internet,
        on_power: cfg.alerts.on_power,
        integ,
        live_minutes: cfg.live.max_minutes,
        saved: saved.into(),
        secrets: app.secrets.description.clone(),
        python_ok: python.exists(),
        python: python.display().to_string(),
        data_dir: cfg.server.data_dir.display().to_string(),
        config_path: app.cfg_path.display().to_string(),
    };
    let header = header(&app);
    let chrome = Chrome::full(&app, &sess, "Settings", "settings");
    match (SettingsPage { chrome, header, s }).render() {
        Ok(h) => Html(h).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("template error: {e}")).into_response(),
    }
}

/// Apply a config change: validate, write umbra.toml, swap it in, restart checks.
fn commit(app: &Shared, cfg: Config) -> Result<(), String> {
    cfg.validate()?;
    cfg.save(&app.cfg_path)?;
    *app.cfg.write().unwrap_or_else(|e| e.into_inner()) = cfg;
    crate::scheduler::restart(app);
    Ok(())
}

fn field<'a>(f: &'a FormMap, k: &str) -> &'a str {
    f.get(k).map(|s| s.trim()).unwrap_or("")
}

fn checked(f: &FormMap, k: &str) -> bool {
    f.get(k).is_some_and(|v| v == "on" || v == "true" || v == "1")
}

fn valid_host(h: &str) -> bool {
    !h.is_empty() && h.len() <= 253 && h.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
}

pub async fn save_device(State(app): State<Shared>, Form(f): Form<FormMap>) -> Response {
    let name = field(&f, "name");
    let host = field(&f, "host");
    if name.is_empty() || name.chars().count() > 60 {
        return toast("bad", "Give the device a name (up to 60 characters).");
    }
    if !valid_host(host) {
        return toast("bad", "Enter an IP address like 192.168.1.20, or a hostname.");
    }
    let mac = match field(&f, "mac") {
        "" => None,
        m => match parse_mac(m) {
            Some(b) => Some(crate::util::fmt_mac(&b)),
            None => return toast("bad", "That MAC address doesn't look right. Use the form AA:BB:CC:DD:EE:FF."),
        },
    };
    let interval = match field(&f, "interval") {
        "" => None,
        v => match v.parse::<u64>() {
            Ok(n) if (10..=3600).contains(&n) => Some(n),
            _ => return toast("bad", "Check interval must be between 10 and 3600 seconds."),
        },
    };
    let tcp_port = match field(&f, "tcp_port") {
        "" => None,
        v => match v.parse::<u16>() {
            Ok(n) if n > 0 => Some(n),
            _ => return toast("bad", "TCP port must be between 1 and 65535."),
        },
    };
    let icon = match field(&f, "icon") {
        "" => None,
        i if assets::ICON_NAMES.contains(&i) => Some(i.to_string()),
        _ => None,
    };
    let mut cfg = app.cfg().clone();
    let original = field(&f, "original_id").to_string();
    let id = if !original.is_empty() && cfg.device(&original).is_some() { original.clone() } else { cfg.unique_id(name) };
    let dev = Device {
        id: id.clone(),
        name: name.to_string(),
        kind: Kind::parse(field(&f, "kind")),
        host: host.to_string(),
        mac,
        icon,
        interval,
        tcp_port,
        ping: checked(&f, "ping"),
        down_after: cfg.device(&id).and_then(|d| d.down_after),
        degraded_ms: cfg.device(&id).and_then(|d| d.degraded_ms),
    };
    match cfg.devices.iter_mut().find(|d| d.id == id) {
        Some(d) => *d = dev,
        None => cfg.devices.push(dev),
    }
    match commit(&app, cfg) {
        Ok(()) => hx_redirect("/settings?saved=device#devices"),
        Err(e) => toast("bad", &e),
    }
}

pub async fn delete_device(State(app): State<Shared>, Path(id): Path<String>) -> Response {
    let mut cfg = app.cfg().clone();
    cfg.devices.retain(|d| d.id != id);
    match commit(&app, cfg) {
        Ok(()) => hx_redirect("/settings?saved=deleted#devices"),
        Err(e) => toast("bad", &e),
    }
}

pub async fn save_network(State(app): State<Shared>, Form(f): Form<FormMap>) -> Response {
    let mut cfg = app.cfg().clone();
    let router = field(&f, "router");
    if router != "auto" && !valid_host(router) {
        return toast("bad", "Router must be \"auto\" or an IP address.");
    }
    let num = |k: &str, lo: u64, hi: u64| field(&f, k).parse::<u64>().ok().filter(|v| (lo..=hi).contains(v));
    let (Some(ri), Some(ii), Some(da)) = (num("router_interval", 5, 600), num("internet_interval", 5, 600), num("down_after", 1, 20)) else {
        return toast("bad", "Intervals must be 5–600 s and \"down after\" 1–20 checks.");
    };
    let Some(dm) = field(&f, "degraded_ms").parse::<f64>().ok().filter(|v| (5.0..=5000.0).contains(v)) else {
        return toast("bad", "The slow threshold must be 5–5000 ms.");
    };
    let targets: Vec<String> = field(&f, "internet_targets").split([',', ' ']).map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect();
    if targets.is_empty() || !targets.iter().all(|t| valid_host(t)) {
        return toast("bad", "List one or more internet addresses, like 1.1.1.1, 8.8.8.8.");
    }
    cfg.network.router = router.to_string();
    cfg.network.router_interval = ri;
    cfg.network.internet_interval = ii;
    cfg.network.down_after = da as u32;
    cfg.network.degraded_ms = dm;
    cfg.network.internet_targets = targets;
    match commit(&app, cfg) {
        Ok(()) => hx_redirect("/settings?saved=network#network"),
        Err(e) => toast("bad", &e),
    }
}

pub async fn save_alerts(State(app): State<Shared>, Form(f): Form<FormMap>) -> Response {
    let mut cfg = app.cfg().clone();
    let server = field(&f, "ntfy_server").trim_end_matches('/');
    if !server.is_empty() && !(server.starts_with("https://") || server.starts_with("http://")) {
        return toast("bad", "The ntfy server must start with https://");
    }
    let topic = field(&f, "ntfy_topic");
    if !topic.is_empty() {
        if topic.len() > 64 || !topic.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return toast("bad", "ntfy topics use letters, numbers, - and _ (up to 64).");
        }
        if let Err(e) = app.secrets.set("ntfy_topic", topic) {
            return toast("bad", &format!("Couldn't save to the keyring: {e}"));
        }
    }
    let token = field(&f, "ntfy_token");
    if !token.is_empty() {
        let _ = app.secrets.set("ntfy_token", token);
    }
    let hook = field(&f, "discord_webhook");
    if !hook.is_empty() {
        if !hook.starts_with("https://discord.com/api/webhooks/") && !hook.starts_with("https://discordapp.com/api/webhooks/") {
            return toast("bad", "That isn't a Discord webhook URL (it starts with https://discord.com/api/webhooks/).");
        }
        if let Err(e) = app.secrets.set("discord_webhook", hook) {
            return toast("bad", &format!("Couldn't save to the keyring: {e}"));
        }
    }
    if checked(&f, "forget_ntfy") {
        let _ = app.secrets.delete("ntfy_topic");
        let _ = app.secrets.delete("ntfy_token");
    }
    if checked(&f, "forget_discord") {
        let _ = app.secrets.delete("discord_webhook");
    }
    cfg.alerts.ntfy = checked(&f, "ntfy");
    if !server.is_empty() {
        cfg.alerts.ntfy_server = server.to_string();
    }
    cfg.alerts.discord = checked(&f, "discord");
    cfg.alerts.on_devices = checked(&f, "on_devices");
    cfg.alerts.on_internet = checked(&f, "on_internet");
    cfg.alerts.on_power = checked(&f, "on_power");
    if let Some(m) = field(&f, "live_minutes").parse::<u64>().ok().filter(|m| (1..=60).contains(m)) {
        cfg.live.max_minutes = m;
    }
    match commit(&app, cfg) {
        Ok(()) => hx_redirect("/settings?saved=alerts#alerts"),
        Err(e) => toast("bad", &e),
    }
}

pub async fn test_alerts(State(app): State<Shared>) -> Response {
    let results = crate::alerts::send_test(&app).await;
    if results.is_empty() {
        return toast("bad", "Turn on a channel and save its topic or webhook first.");
    }
    let msg = results.iter().map(|(ch, r)| match r {
        Ok(()) => format!("{ch}: sent"),
        Err(e) => format!("{ch}: failed ({e})"),
    });
    let all_ok = results.iter().all(|(_, r)| r.is_ok());
    toast(if all_ok { "ok" } else { "bad" }, &msg.collect::<Vec<_>>().join(" · "))
}

pub async fn change_password(State(app): State<Shared>, Form(f): Form<FormMap>) -> Response {
    let (cur, new, again) = (field(&f, "current"), field(&f, "new"), field(&f, "again"));
    let app2 = app.clone();
    let cur_owned = cur.to_string();
    let ok = tokio::task::spawn_blocking(move || crate::web::auth::check_current(&app2, &cur_owned)).await.unwrap_or(false);
    if !ok {
        return toast("bad", "The current password didn't match.");
    }
    if new != again {
        return toast("bad", "The new passwords don't match.");
    }
    let app3 = app.clone();
    let new_owned = new.to_string();
    match tokio::task::spawn_blocking(move || crate::web::auth::set_password(&app3.db, &new_owned)).await {
        Ok(Ok(())) => {
            app.sessions.lock().unwrap_or_else(|e| e.into_inner()).clear();
            hx_redirect("/login")
        }
        Ok(Err(e)) => toast("bad", &e),
        Err(_) => toast("bad", "Something went wrong."),
    }
}

fn valid_integration(name: &str) -> bool {
    name == "ring" || name == "blink"
}

pub async fn integ_login(State(app): State<Shared>, Path(name): Path<String>, Form(f): Form<FormMap>) -> Response {
    if !valid_integration(&name) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let (user, pass) = (field(&f, "username"), f.get("password").map(String::as_str).unwrap_or(""));
    if user.is_empty() || pass.is_empty() {
        return Html(login_form(&name, "Enter your email and password.")).into_response();
    }
    match integrations::login_start(&app, &name, user, pass).await {
        LoginOutcome::Done => hx_redirect(&format!("/settings?saved={name}#integ-{name}")),
        LoginOutcome::NeedCode(hint) => Html(code_form(&name, &hint)).into_response(),
        LoginOutcome::Failed(e) => Html(login_form(&name, &e)).into_response(),
    }
}

pub async fn integ_code(State(app): State<Shared>, Path(name): Path<String>, Form(f): Form<FormMap>) -> Response {
    if !valid_integration(&name) {
        return StatusCode::NOT_FOUND.into_response();
    }
    match integrations::login_code(&app, &name, field(&f, "code")).await {
        LoginOutcome::Done => hx_redirect(&format!("/settings?saved={name}#integ-{name}")),
        LoginOutcome::NeedCode(hint) => Html(code_form(&name, &hint)).into_response(),
        LoginOutcome::Failed(e) => Html(login_form(&name, &e)).into_response(),
    }
}

pub async fn integ_disconnect(State(app): State<Shared>, Path(name): Path<String>) -> Response {
    if !valid_integration(&name) {
        return StatusCode::NOT_FOUND.into_response();
    }
    integrations::disconnect(&app, &name);
    hx_redirect("/settings?saved=disconnected#cameras")
}

pub async fn integ_poll(State(app): State<Shared>, Path(name): Path<String>) -> Response {
    if !valid_integration(&name) {
        return StatusCode::NOT_FOUND.into_response();
    }
    {
        let mut st = app.integ.lock().unwrap_or_else(|e| e.into_inner());
        let s = st.get_mut(&name);
        if !s.needs_login() {
            s.fails = 0;
        }
    }
    if name == "ring" { app.ring_wake.notify_one() } else { app.blink_wake.notify_one() }
    toast("ok", &format!("Checking {} now. Results show up on the dashboard in a few seconds.", integrations::title(&name)))
}
