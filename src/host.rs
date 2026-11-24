//! The server laptop itself: AC vs battery (power-outage alerts), battery wear,
//! CPU temperature, disk and uptime. Power is read every 10 s from sysfs, which
//! costs microseconds; the rest is read only when a page is rendered.

use crate::state::Shared;
use crate::util::now;
use std::path::Path;
use std::time::Duration;

#[derive(Clone, Debug, Default)]
pub struct HostStatus {
    pub on_ac: Option<bool>,
    pub has_battery: bool,
    pub battery_pct: Option<f64>,
    pub battery_state: Option<String>,
    pub full_wh: Option<f64>,
    pub design_wh: Option<f64>,
    /// When the current power state began (outage start, or service start).
    pub since: Option<i64>,
}

impl HostStatus {
    /// Remaining capacity versus design, in percent.
    pub fn health_pct(&self) -> Option<f64> {
        match (self.full_wh, self.design_wh) {
            (Some(f), Some(d)) if d > 0.0 => Some((f / d * 100.0).min(100.0)),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct HostExtra {
    pub hostname: String,
    pub os: String,
    pub cpu_temp_c: Option<f64>,
    pub disk_free: Option<u64>,
    pub disk_total: Option<u64>,
    pub uptime_secs: Option<u64>,
}

pub async fn power_loop(app: Shared) {
    if app.demo {
        // A plugged-in laptop with a slightly worn battery, for the preview.
        *app.host.lock().unwrap_or_else(|e| e.into_inner()) = HostStatus {
            on_ac: Some(true),
            has_battery: true,
            battery_pct: Some(80.0),
            battery_state: Some("Not charging".into()),
            full_wh: Some(47.3),
            design_wh: Some(56.0),
            since: Some(app.started_unix),
        };
        return;
    }
    let mut current: Option<bool> = None;
    let mut candidate: Option<(bool, u8)> = None;
    let mut low_battery_alerted = false;
    // An outage left open from before a restart (the laptop may have died).
    let mut outage: Option<(i64, i64)> = app.db.open_outages().into_iter().find(|o| o.target == "power").map(|o| (o.id, o.started));
    loop {
        let mut st = tokio::task::spawn_blocking(read_power).await.unwrap_or_default();
        let t = now();
        if let Some(ac) = st.on_ac {
            if current.is_none() {
                current = Some(ac);
                if ac {
                    if let Some((id, started)) = outage.take() {
                        app.db.close_outage(id, t);
                        crate::alerts::power_back(&app, started, t);
                    }
                } else if outage.is_none() {
                    outage = Some((app.db.open_outage("power", t), t));
                    if app.cfg().alerts.on_power {
                        crate::alerts::power_out(&app, t, st.battery_pct);
                    }
                }
                app.changed();
            } else if current != Some(ac) {
                // Require two readings in a row (≈10 s) before believing a change.
                let n = match candidate {
                    Some((v, n)) if v == ac => n + 1,
                    _ => 1,
                };
                candidate = Some((ac, n));
                if n >= 2 {
                    current = Some(ac);
                    candidate = None;
                    if ac {
                        if let Some((id, started)) = outage.take() {
                            app.db.close_outage(id, t);
                            if app.cfg().alerts.on_power {
                                crate::alerts::power_back(&app, started, t);
                            }
                        }
                        low_battery_alerted = false;
                    } else {
                        outage = Some((app.db.open_outage("power", t), t));
                        if app.cfg().alerts.on_power {
                            crate::alerts::power_out(&app, t, st.battery_pct);
                        }
                    }
                    app.changed();
                }
            } else {
                candidate = None;
            }
        }
        if current == Some(false) && !low_battery_alerted && st.battery_pct.is_some_and(|p| p <= 15.0) {
            low_battery_alerted = true;
            crate::alerts::info(&app, "Server battery low", &format!("{:.0}% left. Umbra stops when it runs out.", st.battery_pct.unwrap_or(0.0)), 5);
        }
        st.since = outage.map(|o| o.1).or(Some(app.started_unix));
        let changed_pct = {
            let mut h = app.host.lock().unwrap_or_else(|e| e.into_inner());
            let moved = h.battery_pct.map(|p| p.round()) != st.battery_pct.map(|p| p.round());
            *h = st;
            moved
        };
        if changed_pct && current == Some(false) {
            app.changed();
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

#[cfg(target_os = "linux")]
pub fn read_power() -> HostStatus {
    use std::fs;
    let rd = |p: &Path| fs::read_to_string(p).ok().map(|s| s.trim().to_string());
    let num = |p: &Path| rd(p).and_then(|s| s.parse::<f64>().ok());
    let mut st = HostStatus::default();
    let mut saw_supply = false;
    let mut any_online = false;
    if let Ok(dir) = fs::read_dir("/sys/class/power_supply") {
        for e in dir.flatten() {
            let p = e.path();
            match rd(&p.join("type")).as_deref() {
                Some("Mains") | Some("USB") => {
                    if let Some(o) = rd(&p.join("online")) {
                        saw_supply = true;
                        any_online |= o == "1";
                    }
                }
                Some("Battery") => {
                    if rd(&p.join("scope")).as_deref() == Some("Device") {
                        continue; // a mouse or keyboard battery
                    }
                    st.has_battery = true;
                    st.battery_pct = num(&p.join("capacity"));
                    st.battery_state = rd(&p.join("status"));
                    if let (Some(f), Some(d)) = (num(&p.join("energy_full")), num(&p.join("energy_full_design"))) {
                        st.full_wh = Some(f / 1e6);
                        st.design_wh = Some(d / 1e6);
                    } else if let (Some(f), Some(d)) = (num(&p.join("charge_full")), num(&p.join("charge_full_design"))) {
                        let v = num(&p.join("voltage_min_design")).unwrap_or(0.0) / 1e6;
                        if v > 0.0 {
                            st.full_wh = Some(f / 1e6 * v);
                            st.design_wh = Some(d / 1e6 * v);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    st.on_ac = if saw_supply {
        Some(any_online)
    } else {
        st.battery_state.as_deref().map(|s| s != "Discharging")
    };
    st
}

#[cfg(windows)]
pub fn read_power() -> HostStatus {
    use windows_sys::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
    let mut s: SYSTEM_POWER_STATUS = unsafe { std::mem::zeroed() };
    if unsafe { GetSystemPowerStatus(&mut s) } == 0 {
        return HostStatus::default();
    }
    let has_battery = s.BatteryFlag != 128 && s.BatteryFlag != 255;
    HostStatus {
        on_ac: match s.ACLineStatus {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        },
        has_battery,
        battery_pct: (has_battery && s.BatteryLifePercent <= 100).then_some(f64::from(s.BatteryLifePercent)),
        battery_state: has_battery.then(|| if s.BatteryFlag & 8 != 0 { "Charging" } else if s.ACLineStatus == 1 { "Not charging" } else { "Discharging" }.to_string()),
        ..Default::default()
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn read_power() -> HostStatus {
    HostStatus::default()
}

pub fn extra(data_dir: &Path) -> HostExtra {
    HostExtra { hostname: hostname(), os: os_name(), cpu_temp_c: cpu_temp(), uptime_secs: uptime(), ..disk(data_dir) }
}

#[cfg(target_os = "linux")]
fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname").map(|s| s.trim().to_string()).unwrap_or_default()
}
#[cfg(not(target_os = "linux"))]
fn hostname() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_default()
}

fn os_name() -> String {
    if cfg!(windows) {
        return "Windows".into();
    }
    std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|s| s.lines().find_map(|l| l.strip_prefix("PRETTY_NAME=").map(|v| v.trim_matches('"').to_string())))
        .unwrap_or_else(|| "Linux".into())
}

#[cfg(target_os = "linux")]
fn cpu_temp() -> Option<f64> {
    use std::fs;
    let preferred = ["coretemp", "k10temp", "zenpower", "cpu_thermal", "acpitz", "thinkpad"];
    let mut found: Vec<(usize, f64)> = vec![];
    if let Ok(dir) = fs::read_dir("/sys/class/hwmon") {
        for e in dir.flatten() {
            let p = e.path();
            let name = fs::read_to_string(p.join("name")).unwrap_or_default();
            if let Some(rank) = preferred.iter().position(|n| *n == name.trim()) {
                if let Some(v) = fs::read_to_string(p.join("temp1_input")).ok().and_then(|s| s.trim().parse::<f64>().ok()) {
                    found.push((rank, v / 1000.0));
                }
            }
        }
    }
    found.sort_by_key(|f| f.0);
    found.first().map(|f| f.1).filter(|t| *t > 0.0 && *t < 130.0)
}
#[cfg(not(target_os = "linux"))]
fn cpu_temp() -> Option<f64> {
    None // Windows doesn't expose CPU temperature without a kernel driver.
}

#[cfg(target_os = "linux")]
fn uptime() -> Option<u64> {
    std::fs::read_to_string("/proc/uptime").ok()?.split_whitespace().next()?.parse::<f64>().ok().map(|v| v as u64)
}
#[cfg(windows)]
fn uptime() -> Option<u64> {
    Some(unsafe { windows_sys::Win32::System::SystemInformation::GetTickCount64() } / 1000)
}
#[cfg(not(any(windows, target_os = "linux")))]
fn uptime() -> Option<u64> {
    None
}

#[cfg(unix)]
fn disk(path: &Path) -> HostExtra {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else { return HostExtra::default() };
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return HostExtra::default();
    }
    #[allow(clippy::unnecessary_cast)]
    let frsize = s.f_frsize as u64;
    #[allow(clippy::unnecessary_cast)]
    HostExtra { disk_free: Some(s.f_bavail as u64 * frsize), disk_total: Some(s.f_blocks as u64 * frsize), ..Default::default() }
}

#[cfg(windows)]
fn disk(path: &Path) -> HostExtra {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let abs = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let wide: Vec<u16> = abs.as_os_str().encode_wide().chain(Some(0)).collect();
    let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
    if unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut avail, &mut total, &mut free) } == 0 {
        return HostExtra::default();
    }
    HostExtra { disk_free: Some(avail), disk_total: Some(total), ..Default::default() }
}
