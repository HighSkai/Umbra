//! Ring and Blink have no official API; the only maintained clients are Python
//! (`ring_doorbell`, `blinkpy`). Each poll starts a short-lived helper process:
//! the core writes the token to its stdin, the helper prints JSON lines to
//! stdout and exits. No Python process stays resident, except during an
//! interactive login (waiting for the 2FA code) or a live view you started.
//!
//! A failing helper never touches the core: it times out, gets killed, and the
//! integration backs off and shows a banner ("Ring: login expired, reconnect").

use crate::db::CamEvent;
use crate::state::{App, Shared};
use crate::stats::HelperRun;
use crate::util::now;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout};

pub const SOURCES: [&str; 2] = ["ring", "blink"];

pub fn title(name: &str) -> &'static str {
    match name {
        "ring" => "Ring",
        "blink" => "Blink",
        _ => "Integration",
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Camera {
    pub source: String,
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub model: String,
    pub online: Option<bool>,
    pub battery: Option<f64>,
    pub wired: Option<bool>,
    pub wifi_rssi: Option<i64>,
    pub wifi_bars: Option<i64>,
    pub armed: Option<bool>,
    pub last_ding: Option<i64>,
    pub last_motion: Option<i64>,
    #[serde(default)]
    pub snapshot_at: Option<i64>,
    #[serde(default)]
    pub doorbell: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Source {
    pub last_poll: Option<i64>,
    pub last_ok: Option<i64>,
    /// Machine-readable error: auth_expired, need_login, api_changed, rate_limited, network, helper_missing, ...
    pub error: Option<String>,
    pub message: Option<String>,
    pub cameras: Vec<Camera>,
    #[serde(default)]
    pub fails: u32,
}

impl Source {
    pub fn needs_login(&self) -> bool {
        matches!(self.error.as_deref(), Some("auth_expired" | "need_login"))
    }
}

#[derive(Default)]
pub struct IntegState {
    pub ring: Source,
    pub blink: Source,
    pub snapshot_requests: HashMap<String, i64>,
}

impl IntegState {
    pub fn get(&self, name: &str) -> &Source {
        if name == "ring" { &self.ring } else { &self.blink }
    }
    pub fn get_mut(&mut self, name: &str) -> &mut Source {
        if name == "ring" { &mut self.ring } else { &mut self.blink }
    }
}

fn token_key(name: &str) -> String {
    format!("{name}_token")
}

/// Stored credential: the token object plus the hardware id we registered with.
#[derive(Serialize, Deserialize, Default)]
struct Stored {
    token: Value,
    hardware_id: String,
}

fn load_stored(app: &App, name: &str) -> Option<Stored> {
    serde_json::from_str(&app.secrets.get(&token_key(name))?).ok()
}

pub fn connected(app: &App, name: &str) -> bool {
    load_stored(app, name).is_some_and(|s| !s.token.is_null())
}

fn save_token(app: &App, name: &str, token: &Value) {
    let hw = load_stored(app, name).map(|s| s.hardware_id).unwrap_or_default();
    let stored = Stored { token: token.clone(), hardware_id: hw };
    if let Err(e) = app.secrets.set(&token_key(name), &serde_json::to_string(&stored).unwrap_or_default()) {
        crate::error!("{name}: saving refreshed token failed: {e}");
    }
}

fn helper_input(app: &App, name: &str, extra: Value) -> Option<Value> {
    let s = load_stored(app, name)?;
    let mut v = json!({ "token": s.token, "hardware_id": s.hardware_id });
    if let (Some(o), Some(e)) = (v.as_object_mut(), extra.as_object()) {
        for (k, val) in e {
            o.insert(k.clone(), val.clone());
        }
    }
    Some(v)
}

// ---------------------------------------------------------------------------
// Helper process plumbing
// ---------------------------------------------------------------------------

pub struct Helper {
    pub name: String,
    pub cmd: String,
    pub child: Child,
    pub stdin: Option<ChildStdin>,
    pub out: Option<Lines<BufReader<ChildStdout>>>,
    pub started: Instant,
    pub stderr_tail: Arc<Mutex<VecDeque<String>>>,
}

impl Helper {
    pub async fn spawn(app: &App, name: &str, cmd: &str, input: &Value) -> Result<Helper, String> {
        let (python, script) = {
            let cfg = app.cfg();
            (cfg.server.python_path(), cfg.server.helpers_dir.join(format!("{name}_helper.py")))
        };
        if !python.exists() {
            return Err(format!("The Python helper environment isn't installed (looked for {}). Run the install script.", python.display()));
        }
        if !script.exists() {
            return Err(format!("Missing helper {}", script.display()));
        }
        let mut c = tokio::process::Command::new(&python);
        // -I: isolated (ignores PYTHON* env vars and the user site); -u: unbuffered.
        c.arg("-I").arg("-u").arg(&script).arg(cmd);
        c.env_clear();
        for k in ["PATH", "SYSTEMROOT", "SystemRoot", "WINDIR", "TEMP", "TMP", "HOME", "LANG", "TZ", "USERPROFILE", "LOCALAPPDATA", "APPDATA", "SSL_CERT_FILE"] {
            if let Some(v) = std::env::var_os(k) {
                c.env(k, v);
            }
        }
        c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        let mut child = c.spawn().map_err(|e| format!("starting {}: {e}", python.display()))?;
        let stdin = child.stdin.take();
        let out = child.stdout.take().map(|s| BufReader::new(s).lines());
        let tail: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(VecDeque::new()));
        if let Some(err) = child.stderr.take() {
            let tail2 = tail.clone();
            let tag = format!("{name}/{cmd}");
            tokio::spawn(async move {
                let mut lines = BufReader::new(err).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    crate::util::log("PY", format_args!("[{tag}] {l}"));
                    let mut t = tail2.lock().unwrap_or_else(|e| e.into_inner());
                    t.push_back(l);
                    if t.len() > 8 {
                        t.pop_front();
                    }
                }
            });
        }
        let mut h = Helper { name: name.into(), cmd: cmd.into(), child, stdin, out, started: Instant::now(), stderr_tail: tail };
        h.send(input).await?;
        Ok(h)
    }

    pub async fn send(&mut self, v: &Value) -> Result<(), String> {
        let stdin = self.stdin.as_mut().ok_or("helper stdin closed")?;
        let mut line = v.to_string();
        line.push('\n');
        stdin.write_all(line.as_bytes()).await.map_err(|e| format!("helper stopped: {e}"))?;
        stdin.flush().await.map_err(|e| e.to_string())
    }

    /// Next message that isn't a token refresh (those are saved on the spot).
    pub async fn recv(&mut self, app: &App, timeout: Duration) -> Result<Value, String> {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.checked_duration_since(Instant::now()).ok_or("the helper took too long")?;
            let out = self.out.as_mut().ok_or("helper output closed")?;
            let line = match tokio::time::timeout(left, out.next_line()).await {
                Err(_) => return Err("the helper took too long".into()),
                Ok(Err(e)) => return Err(e.to_string()),
                Ok(Ok(None)) => return Err(self.crash_note()),
                Ok(Ok(Some(l))) => l,
            };
            let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
            if v["type"] == "token" {
                save_token(app, &self.name, &v["token"]);
                continue;
            }
            return Ok(v);
        }
    }

    fn crash_note(&self) -> String {
        let tail = self.stderr_tail.lock().map(|t| t.back().cloned()).ok().flatten();
        match tail {
            Some(l) => format!("the helper exited: {l}"),
            None => "the helper exited without a result".into(),
        }
    }

    pub async fn finish(mut self, app: &App, ok: bool, note: &str) {
        drop(self.stdin.take());
        if tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await.is_err() {
            let _ = self.child.kill().await;
        }
        app.stats.record_helper(HelperRun { name: self.name.clone(), cmd: self.cmd.clone(), at: now(), ms: self.started.elapsed().as_millis() as u64, ok, note: note.chars().take(140).collect() });
    }
}

// ---------------------------------------------------------------------------
// Polling
// ---------------------------------------------------------------------------

pub fn load_state(app: &App) {
    let mut st = app.integ.lock().unwrap_or_else(|e| e.into_inner());
    for name in SOURCES {
        if let Some(src) = app.db.load_integration(name).and_then(|s| serde_json::from_str::<Source>(&s).ok()) {
            *st.get_mut(name) = src;
        }
    }
}

fn persist(app: &App, name: &str) {
    let data = serde_json::to_string(app.integ.lock().unwrap_or_else(|e| e.into_inner()).get(name)).unwrap_or_default();
    app.db.save_integration(name, &data);
}

fn set_error(app: &App, name: &str, code: &str, msg: &str) {
    {
        let mut st = app.integ.lock().unwrap_or_else(|e| e.into_inner());
        let s = st.get_mut(name);
        s.last_poll = Some(now());
        s.error = Some(code.to_string());
        s.message = Some(msg.to_string());
        s.fails += 1;
    }
    crate::warn!("{name}: {code}: {msg}");
    persist(app, name);
    app.changed();
}

pub async fn poll_loop(app: Shared, name: &'static str) {
    if app.demo {
        return;
    }
    tokio::time::sleep(Duration::from_secs(if name == "ring" { 20 } else { 35 })).await;
    loop {
        let (enabled, interval) = {
            let c = app.cfg();
            if name == "ring" { (c.ring.enabled, c.ring_interval()) } else { (c.blink.enabled, c.blink_interval()) }
        };
        let blocked = app.integ.lock().map(|s| s.get(name).needs_login()).unwrap_or(false);
        let mut ok = true;
        if enabled && connected(&app, name) && !blocked {
            ok = poll_once(&app, name).await;
        }
        let fails = app.integ.lock().map(|s| s.get(name).fails).unwrap_or(0);
        let delay = if ok || fails == 0 { interval } else { (interval << fails.min(5)).min(6 * 3600) };
        let wake = if name == "ring" { &app.ring_wake } else { &app.blink_wake };
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(delay)) => {}
            _ = wake.notified() => {}
        }
    }
}

pub async fn poll_once(app: &App, name: &str) -> bool {
    let Some(input) = helper_input(app, name, json!({})) else { return false };
    let mut h = match Helper::spawn(app, name, "poll", &input).await {
        Ok(h) => h,
        Err(e) => {
            set_error(app, name, "helper_missing", &e);
            return false;
        }
    };
    let res = h.recv(app, Duration::from_secs(120)).await;
    let (ok, note) = match &res {
        Ok(v) if v["type"] == "result" => (true, "ok".to_string()),
        Ok(v) => (false, v["code"].as_str().unwrap_or("error").to_string()),
        Err(e) => (false, e.clone()),
    };
    h.finish(app, ok, &note).await;
    match res {
        Ok(v) if v["type"] == "result" => {
            apply_result(app, name, &v);
            true
        }
        Ok(v) => {
            let code = v["code"].as_str().unwrap_or("unknown");
            set_error(app, name, code, v["message"].as_str().unwrap_or("unknown error"));
            false
        }
        Err(e) => {
            set_error(app, name, "helper_failed", &e);
            false
        }
    }
}

fn apply_result(app: &App, name: &str, v: &Value) {
    let mut cams = vec![];
    let mut new_events = 0;
    for c in v["cameras"].as_array().cloned().unwrap_or_default() {
        let id = match &c["id"] {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            _ => continue,
        };
        for e in c["events"].as_array().cloned().unwrap_or_default() {
            if let (Some(kind), Some(ts)) = (e["kind"].as_str(), e["ts"].as_i64())
                && app.db.insert_cam_event(&CamEvent { source: name.into(), device: id.clone(), kind: kind.into(), ts }) {
                    new_events += 1;
                }
        }
        cams.push(Camera {
            source: name.into(),
            id,
            name: c["name"].as_str().unwrap_or("Camera").into(),
            model: c["model"].as_str().unwrap_or_default().into(),
            online: c["online"].as_bool(),
            battery: c["battery"].as_f64(),
            wired: c["wired"].as_bool(),
            wifi_rssi: c["wifi_rssi"].as_i64(),
            wifi_bars: c["wifi_bars"].as_i64(),
            armed: c["armed"].as_bool(),
            last_ding: c["last_ding"].as_i64(),
            last_motion: c["last_motion"].as_i64(),
            snapshot_at: None,
            doorbell: c["doorbell"].as_bool().unwrap_or(false),
        });
    }
    let changed;
    {
        let mut st = app.integ.lock().unwrap_or_else(|e| e.into_inner());
        let s = st.get_mut(name);
        for c in &mut cams {
            c.snapshot_at = s.cameras.iter().find(|o| o.id == c.id).and_then(|o| o.snapshot_at);
        }
        changed = new_events > 0
            || s.error.is_some()
            || s.cameras.len() != cams.len()
            || s.cameras.iter().zip(&cams).any(|(a, b)| a.online != b.online || a.armed != b.armed || a.battery.map(f64::round) != b.battery.map(f64::round));
        s.cameras = cams;
        s.last_poll = Some(now());
        s.last_ok = s.last_poll;
        s.error = None;
        s.message = None;
        s.fails = 0;
    }
    persist(app, name);
    if changed {
        app.changed();
    }
}

// ---------------------------------------------------------------------------
// Login (once, with 2FA) and disconnect
// ---------------------------------------------------------------------------

pub struct LoginFlow {
    helper: Helper,
    started: Instant,
}

pub enum LoginOutcome {
    Done,
    NeedCode(String),
    Failed(String),
}

pub async fn login_start(app: &Shared, name: &str, username: &str, password: &str) -> LoginOutcome {
    // Reuse the hardware id if we've logged in before, so Ring/Blink see one device.
    let hw = load_stored(app, name).map(|s| s.hardware_id).filter(|h| !h.is_empty()).unwrap_or_else(|| {
        let u = crate::util::uuid4();
        if name == "blink" { u.to_uppercase() } else { u }
    });
    let _ = app.secrets.set(&token_key(name), &serde_json::to_string(&Stored { token: load_stored(app, name).map(|s| s.token).unwrap_or(Value::Null), hardware_id: hw.clone() }).unwrap_or_default());
    if let Some(old) = app.logins.lock().await.remove(name) {
        old.helper.finish(app, false, "replaced").await;
    }
    let input = json!({ "username": username, "password": password, "hardware_id": hw });
    let helper = match Helper::spawn(app, name, "login", &input).await {
        Ok(h) => h,
        Err(e) => return LoginOutcome::Failed(e),
    };
    continue_login(app, name, helper).await
}

pub async fn login_code(app: &Shared, name: &str, code: &str) -> LoginOutcome {
    let Some(mut flow) = app.logins.lock().await.remove(name) else {
        return LoginOutcome::Failed("That sign-in expired. Start again.".into());
    };
    if flow.started.elapsed() > Duration::from_secs(600) {
        flow.helper.finish(app, false, "expired").await;
        return LoginOutcome::Failed("That sign-in expired. Start again.".into());
    }
    let digits: String = code.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    if let Err(e) = flow.helper.send(&json!({ "code": digits })).await {
        return LoginOutcome::Failed(e);
    }
    continue_login(app, name, flow.helper).await
}

async fn continue_login(app: &Shared, name: &str, mut helper: Helper) -> LoginOutcome {
    loop {
        match helper.recv(app, Duration::from_secs(90)).await {
            Ok(v) => match v["type"].as_str() {
                Some("need_2fa") => {
                    let hint = v["message"].as_str().unwrap_or("Enter the code that was just sent to you.").to_string();
                    app.logins.lock().await.insert(name.to_string(), LoginFlow { helper, started: Instant::now() });
                    return LoginOutcome::NeedCode(hint);
                }
                Some("done") => {
                    helper.finish(app, true, "signed in").await;
                    {
                        let mut st = app.integ.lock().unwrap_or_else(|e| e.into_inner());
                        let s = st.get_mut(name);
                        s.error = None;
                        s.message = None;
                        s.fails = 0;
                    }
                    persist(app, name);
                    enable(app, name, true);
                    if name == "ring" { app.ring_wake.notify_one() } else { app.blink_wake.notify_one() }
                    return LoginOutcome::Done;
                }
                Some("error") => {
                    let msg = v["message"].as_str().unwrap_or("Sign-in failed").to_string();
                    helper.finish(app, false, &msg).await;
                    return LoginOutcome::Failed(msg);
                }
                _ => continue,
            },
            Err(e) => {
                helper.finish(app, false, &e).await;
                return LoginOutcome::Failed(e);
            }
        }
    }
}

fn enable(app: &App, name: &str, on: bool) {
    let mut cfg = app.cfg().clone();
    if name == "ring" {
        cfg.ring.enabled = on;
    } else {
        cfg.blink.enabled = on;
    }
    if cfg.save(&app.cfg_path).is_ok() {
        *app.cfg.write().unwrap_or_else(|e| e.into_inner()) = cfg;
    }
}

pub fn disconnect(app: &App, name: &str) {
    let _ = app.secrets.delete(&token_key(name));
    *app.integ.lock().unwrap_or_else(|e| e.into_inner()).get_mut(name) = Source::default();
    persist(app, name);
    enable(app, name, false);
    app.changed();
}

// ---------------------------------------------------------------------------
// Blink snapshot (wired cameras only; no battery cost)
// ---------------------------------------------------------------------------

pub fn snapshot_path(app: &App, source: &str, id: &str) -> std::path::PathBuf {
    let safe: String = id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
    app.cfg().server.data_dir.join("snapshots").join(format!("{source}-{safe}.jpg"))
}

pub async fn snapshot(app: &App, camera_id: &str) -> Result<(), String> {
    {
        let mut st = app.integ.lock().unwrap_or_else(|e| e.into_inner());
        if !st.blink.cameras.iter().any(|c| c.id == camera_id) {
            return Err("Unknown camera".into());
        }
        let last = st.snapshot_requests.get(camera_id).copied().unwrap_or(0);
        if now() - last < 60 {
            return Err(format!("Blink limits how often cameras can be asked. Try again in {} s.", 60 - (now() - last)));
        }
        st.snapshot_requests.insert(camera_id.to_string(), now());
    }
    let input = helper_input(app, "blink", json!({ "camera_id": camera_id })).ok_or("Blink isn't connected")?;
    let mut h = Helper::spawn(app, "blink", "snapshot", &input).await?;
    let res = h.recv(app, Duration::from_secs(90)).await;
    let ok = matches!(&res, Ok(v) if v["type"] == "snapshot");
    h.finish(app, ok, if ok { "ok" } else { "failed" }).await;
    let v = res?;
    if v["type"] != "snapshot" {
        return Err(v["message"].as_str().unwrap_or("Snapshot failed").to_string());
    }
    use base64::Engine;
    let jpeg = base64::engine::general_purpose::STANDARD.decode(v["jpeg_b64"].as_str().unwrap_or_default()).map_err(|e| e.to_string())?;
    let path = snapshot_path(app, "blink", camera_id);
    if let Some(d) = path.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    std::fs::write(&path, jpeg).map_err(|e| e.to_string())?;
    {
        let mut st = app.integ.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = st.blink.cameras.iter_mut().find(|c| c.id == camera_id) {
            c.snapshot_at = Some(now());
        }
    }
    persist(app, "blink");
    app.changed();
    Ok(())
}

pub fn input_for_live(app: &App, name: &str, camera_id: &str) -> Option<Value> {
    helper_input(app, name, json!({ "camera_id": camera_id }))
}
