//! Self-measurement for `/debug/stats`: CPU %, memory, checks per minute,
//! helper run times and database size.

use crate::state::Shared;
use crate::util::now;
use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct HelperRun {
    pub name: String,
    pub cmd: String,
    pub at: i64,
    pub ms: u64,
    pub ok: bool,
    pub note: String,
}

pub struct Stats {
    pub checks_total: AtomicU64,
    minutes: Mutex<VecDeque<(i64, u32)>>,
    cpu: Mutex<VecDeque<(Instant, f64)>>,
    pub helpers: Mutex<VecDeque<HelperRun>>,
    pub sse_clients: AtomicUsize,
    pub requests: AtomicU64,
}

impl Stats {
    pub fn new() -> Stats {
        Stats {
            checks_total: AtomicU64::new(0),
            minutes: Mutex::new(VecDeque::new()),
            cpu: Mutex::new(VecDeque::new()),
            helpers: Mutex::new(VecDeque::new()),
            sse_clients: AtomicUsize::new(0),
            requests: AtomicU64::new(0),
        }
    }

    pub fn count_check(&self) {
        self.checks_total.fetch_add(1, Ordering::Relaxed);
        let m = now() / 60;
        let mut q = self.minutes.lock().unwrap_or_else(|e| e.into_inner());
        match q.back_mut() {
            Some((mm, n)) if *mm == m => *n += 1,
            _ => q.push_back((m, 1)),
        }
        while q.len() > 16 {
            q.pop_front();
        }
    }

    /// Average over the last 10 complete minutes.
    pub fn checks_per_min(&self) -> f64 {
        let m = now() / 60;
        let q = self.minutes.lock().unwrap_or_else(|e| e.into_inner());
        let sum: u32 = q.iter().filter(|(mm, _)| *mm < m && *mm >= m - 10).map(|(_, n)| *n).sum();
        let span = (m - q.front().map(|f| f.0).unwrap_or(m)).clamp(1, 10);
        f64::from(sum) / span as f64
    }

    pub fn record_helper(&self, run: HelperRun) {
        let mut q = self.helpers.lock().unwrap_or_else(|e| e.into_inner());
        q.push_front(run);
        q.truncate(30);
    }

    pub fn sample_cpu(&self) {
        if let Some(c) = process_cpu_secs() {
            let mut q = self.cpu.lock().unwrap_or_else(|e| e.into_inner());
            q.push_back((Instant::now(), c));
            while q.len() > 11 {
                q.pop_front();
            }
        }
    }

    /// CPU % of one core over up to the last 10 minutes of samples.
    pub fn cpu_pct_window(&self) -> Option<(f64, u64)> {
        let q = self.cpu.lock().unwrap_or_else(|e| e.into_inner());
        let (t0, c0) = *q.front()?;
        let now_c = process_cpu_secs()?;
        let wall = t0.elapsed().as_secs_f64();
        (wall > 5.0).then(|| ((now_c - c0) / wall * 100.0, wall as u64))
    }
}

pub async fn sampler_loop(app: Shared) {
    loop {
        app.stats.sample_cpu();
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}

#[derive(Clone, Debug, Default)]
pub struct Mem {
    /// Resident set (Linux) or working set (Windows).
    pub rss: Option<u64>,
    /// Anonymous/private memory: what the process itself allocated.
    pub private: Option<u64>,
    pub threads: Option<u64>,
}

#[cfg(target_os = "linux")]
pub fn process_cpu_secs() -> Option<f64> {
    let s = std::fs::read_to_string("/proc/self/stat").ok()?;
    let rest = &s[s.rfind(')')? + 2..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    let utime: f64 = f.get(11)?.parse().ok()?;
    let stime: f64 = f.get(12)?.parse().ok()?;
    Some((utime + stime) / 100.0) // USER_HZ is 100 on Linux
}

#[cfg(target_os = "linux")]
pub fn memory() -> Mem {
    let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let field = |k: &str| s.lines().find_map(|l| l.strip_prefix(k)).and_then(|v| v.split_whitespace().next()).and_then(|v| v.parse::<u64>().ok());
    Mem { rss: field("VmRSS:").map(|k| k * 1024), private: field("RssAnon:").map(|k| k * 1024), threads: field("Threads:") }
}

#[cfg(windows)]
pub fn process_cpu_secs() -> Option<f64> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let z = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut c, mut e, mut k, mut u) = (z, z, z, z);
    if unsafe { GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u) } == 0 {
        return None;
    }
    let t = |f: FILETIME| ((u64::from(f.dwHighDateTime) << 32) | u64::from(f.dwLowDateTime)) as f64 / 1e7;
    Some(t(k) + t(u))
}

#[cfg(windows)]
pub fn memory() -> Mem {
    use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut pmc: PROCESS_MEMORY_COUNTERS_EX = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
    pmc.cb = size;
    if unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut pmc as *mut _ as *mut PROCESS_MEMORY_COUNTERS, size) } == 0 {
        return Mem::default();
    }
    Mem { rss: Some(pmc.WorkingSetSize as u64), private: Some(pmc.PrivateUsage as u64), threads: None }
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn process_cpu_secs() -> Option<f64> {
    None
}
#[cfg(not(any(windows, target_os = "linux")))]
pub fn memory() -> Mem {
    Mem::default()
}
