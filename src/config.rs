//! `umbra.toml`: devices, intervals and integrations. Secrets never live here.

use crate::util;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const RESERVED_IDS: &[&str] = &["router", "internet", "power", "server", "ring", "blink", "thermostat"];

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub server: Server,
    pub network: Network,
    pub alerts: Alerts,
    pub live: Live,
    pub ring: Integration,
    pub blink: Integration,
    pub thermostat: Thermostat,
    #[serde(rename = "device", skip_serializing_if = "Vec::is_empty")]
    pub devices: Vec<Device>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Server {
    /// Shown in the page title.
    pub name: String,
    /// LAN address to listen on. Requests from public IPs are refused regardless.
    pub bind: String,
    pub data_dir: PathBuf,
    pub helpers_dir: PathBuf,
    /// Python inside the helpers venv. Defaults to `<helpers_dir>/.venv/...`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub python: Option<PathBuf>,
    /// Extra CIDRs allowed to reach the UI besides private ranges (e.g. a VPN).
    pub allow_cidrs: Vec<String>,
}

impl Default for Server {
    fn default() -> Self {
        Self {
            name: "Home".into(),
            bind: "0.0.0.0:8770".into(),
            data_dir: default_data_dir(),
            helpers_dir: default_helpers_dir(),
            python: None,
            allow_cidrs: vec![],
        }
    }
}

impl Server {
    pub fn python_path(&self) -> PathBuf {
        if let Some(p) = &self.python {
            return p.clone();
        }
        if cfg!(windows) {
            self.helpers_dir.join(".venv").join("Scripts").join("python.exe")
        } else {
            self.helpers_dir.join(".venv").join("bin").join("python3")
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Network {
    /// Router address, or "auto" to use the default gateway.
    pub router: String,
    pub router_interval: u64,
    pub internet_targets: Vec<String>,
    pub internet_interval: u64,
    pub ping_timeout_ms: u64,
    /// Consecutive failed checks before a device counts as down.
    pub down_after: u32,
    /// Latency above this marks a device as degraded.
    pub degraded_ms: f64,
    /// Packet loss over the last 10 checks above this marks it degraded.
    pub degraded_loss_pct: f64,
}

impl Default for Network {
    fn default() -> Self {
        Self {
            router: "auto".into(),
            router_interval: 30,
            internet_targets: vec!["1.1.1.1".into(), "8.8.8.8".into()],
            internet_interval: 30,
            ping_timeout_ms: 1500,
            down_after: 3,
            degraded_ms: 150.0,
            degraded_loss_pct: 30.0,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Alerts {
    /// Phone push through ntfy. The topic lives in the keyring.
    pub ntfy: bool,
    pub ntfy_server: String,
    /// Discord webhook. The URL lives in the keyring.
    pub discord: bool,
    pub on_devices: bool,
    pub on_internet: bool,
    pub on_power: bool,
}

impl Default for Alerts {
    fn default() -> Self {
        Self { ntfy: false, ntfy_server: "https://ntfy.sh".into(), discord: false, on_devices: true, on_internet: true, on_power: true }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Live {
    /// Live views stop on their own after this long.
    pub max_minutes: u64,
    /// ffmpeg is only needed for Blink live view (it repackages, never re-encodes video).
    pub ffmpeg: String,
}

impl Default for Live {
    fn default() -> Self {
        Self { max_minutes: 10, ffmpeg: "ffmpeg".into() }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Integration {
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interval_min: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Thermostat {
    pub enabled: bool,
    pub interval_min: u64,
}

impl Default for Thermostat {
    fn default() -> Self {
        Self { enabled: false, interval_min: 10 }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Computer,
    Server,
    Network,
    Camera,
    Printer,
    Tv,
    Console,
    Phone,
    Tablet,
    Speaker,
    Smarthome,
    #[default]
    Other,
}

pub const KINDS: &[Kind] = &[
    Kind::Computer, Kind::Server, Kind::Network, Kind::Camera, Kind::Printer, Kind::Tv,
    Kind::Console, Kind::Phone, Kind::Tablet, Kind::Speaker, Kind::Smarthome, Kind::Other,
];

impl Kind {
    pub fn key(self) -> &'static str {
        match self {
            Kind::Computer => "computer",
            Kind::Server => "server",
            Kind::Network => "network",
            Kind::Camera => "camera",
            Kind::Printer => "printer",
            Kind::Tv => "tv",
            Kind::Console => "console",
            Kind::Phone => "phone",
            Kind::Tablet => "tablet",
            Kind::Speaker => "speaker",
            Kind::Smarthome => "smarthome",
            Kind::Other => "other",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Kind::Computer => "Computer",
            Kind::Server => "Server / NAS",
            Kind::Network => "Access point / extender",
            Kind::Camera => "Camera",
            Kind::Printer => "Printer",
            Kind::Tv => "TV",
            Kind::Console => "Game console",
            Kind::Phone => "Phone",
            Kind::Tablet => "Tablet",
            Kind::Speaker => "Speaker",
            Kind::Smarthome => "Smart home device",
            Kind::Other => "Other",
        }
    }
    pub fn parse(s: &str) -> Kind {
        KINDS.iter().copied().find(|k| k.key() == s).unwrap_or(Kind::Other)
    }
    pub fn icon(self) -> &'static str {
        match self {
            Kind::Computer => "laptop",
            Kind::Server => "hard-drive",
            Kind::Network => "wifi",
            Kind::Camera => "cctv",
            Kind::Printer => "printer",
            Kind::Tv => "tv",
            Kind::Console => "gamepad-2",
            Kind::Phone => "smartphone",
            Kind::Tablet => "tablet",
            Kind::Speaker => "speaker",
            Kind::Smarthome => "lightbulb",
            Kind::Other => "radio",
        }
    }
    /// Seconds between pings: computers 60 s, cameras and the rest 5 min.
    pub fn interval(self) -> u64 {
        match self {
            Kind::Computer | Kind::Server | Kind::Network => 60,
            _ => 300,
        }
    }
    /// Which group a card lands in on the dashboard.
    pub fn group(self) -> (u8, &'static str) {
        match self {
            Kind::Network => (0, "Network"),
            Kind::Computer | Kind::Server => (1, "Computers"),
            Kind::Camera => (2, "Cameras"),
            Kind::Tv | Kind::Console | Kind::Phone | Kind::Tablet | Kind::Speaker => (3, "Media and phones"),
            Kind::Printer | Kind::Smarthome | Kind::Other => (4, "Other devices"),
        }
    }
    /// Phones sleep their Wi-Fi and drop pings, so they need more misses before "down".
    pub fn default_down_after(self, global: u32) -> u32 {
        match self {
            Kind::Phone | Kind::Tablet => global.max(6),
            _ => global,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Device {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub kind: Kind,
    /// IP address or hostname.
    pub host: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// Seconds between checks (defaults by kind).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tcp_port: Option<u16>,
    /// Set false to skip ICMP (e.g. a battery camera). TCP still runs if set.
    #[serde(default = "yes")]
    pub ping: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub down_after: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degraded_ms: Option<f64>,
}

fn yes() -> bool {
    true
}

impl Device {
    pub fn icon(&self) -> &str {
        self.icon.as_deref().filter(|i| crate::web::assets::ICON_NAMES.contains(i)).unwrap_or(self.kind.icon())
    }
    pub fn interval(&self) -> u64 {
        self.interval.unwrap_or(self.kind.interval()).max(10)
    }
}

fn default_data_dir() -> PathBuf {
    if cfg!(windows) {
        program_data().join("data")
    } else {
        "/var/lib/umbra".into()
    }
}

fn default_helpers_dir() -> PathBuf {
    if cfg!(windows) {
        std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("helpers"))).unwrap_or_else(|| "helpers".into())
    } else {
        "/opt/umbra/helpers".into()
    }
}

fn program_data() -> PathBuf {
    std::env::var_os("ProgramData").map(PathBuf::from).unwrap_or_else(|| "C:\\ProgramData".into()).join("Umbra")
}

/// `--config` flag, then `UMBRA_CONFIG`, then `./umbra.toml` if present, then the OS default.
pub fn resolve_path(flag: Option<PathBuf>) -> PathBuf {
    if let Some(p) = flag {
        return p;
    }
    if let Some(p) = std::env::var_os("UMBRA_CONFIG") {
        return p.into();
    }
    let local = PathBuf::from("umbra.toml");
    if local.exists() {
        return local;
    }
    if cfg!(windows) { program_data().join("umbra.toml") } else { "/etc/umbra/umbra.toml".into() }
}

const HEADER: &str = "# Umbra configuration. Edit by hand or from Settings in the web UI\n\
# (saving from the UI rewrites this file, so comments below this header are not kept).\n\
# Secrets - alert topics, webhooks, Ring/Blink tokens - are never stored here.\n\n";

impl Config {
    pub fn load(path: &Path) -> Result<Config, String> {
        if !path.exists() {
            let cfg = Config::default();
            cfg.save(path)?;
            return Ok(cfg);
        }
        let text = std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
        let mut cfg: Config = toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        cfg.normalize();
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let body = toml::to_string_pretty(self).map_err(|e| e.to_string())?;
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
        }
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, format!("{HEADER}{body}")).map_err(|e| format!("writing {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("replacing {}: {e}", path.display()))
    }

    fn normalize(&mut self) {
        for d in &mut self.devices {
            if d.id.trim().is_empty() {
                d.id = util::slugify(&d.name);
            }
            d.host = d.host.trim().to_string();
            if let Some(m) = &d.mac {
                d.mac = util::parse_mac(m).map(|m| util::fmt_mac(&m));
            }
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        let mut seen = std::collections::HashSet::new();
        for d in &self.devices {
            if d.name.trim().is_empty() {
                return Err(format!("device \"{}\" needs a name", d.id));
            }
            if d.host.is_empty() {
                return Err(format!("device \"{}\" needs an IP address or hostname", d.name));
            }
            if RESERVED_IDS.contains(&d.id.as_str()) {
                return Err(format!("device id \"{}\" is reserved, pick another", d.id));
            }
            if !seen.insert(d.id.clone()) {
                return Err(format!("two devices share the id \"{}\"", d.id));
            }
        }
        if self.network.internet_targets.is_empty() {
            return Err("network.internet_targets needs at least one address".into());
        }
        Ok(())
    }

    pub fn ring_interval(&self) -> u64 {
        self.ring.interval_min.unwrap_or(15).max(5) * 60
    }
    pub fn blink_interval(&self) -> u64 {
        self.blink.interval_min.unwrap_or(12).max(10) * 60
    }

    pub fn device(&self, id: &str) -> Option<&Device> {
        self.devices.iter().find(|d| d.id == id)
    }

    /// A free id derived from the name.
    pub fn unique_id(&self, name: &str) -> String {
        let base = util::slugify(name);
        let base = if RESERVED_IDS.contains(&base.as_str()) { format!("{base}-1") } else { base };
        let mut id = base.clone();
        let mut n = 2;
        while self.devices.iter().any(|d| d.id == id) {
            id = format!("{base}-{n}");
            n += 1;
        }
        id
    }
}
