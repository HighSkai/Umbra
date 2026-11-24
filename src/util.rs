//! Small shared helpers: logging, time formatting, randomness, escaping.

use chrono::{Local, TimeZone};
use std::fmt::Arguments;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn log(level: &str, args: Arguments) {
    eprintln!("{} {level:<5} {args}", Local::now().format("%Y-%m-%d %H:%M:%S"));
}

#[macro_export]
macro_rules! info { ($($t:tt)*) => { $crate::util::log("INFO", format_args!($($t)*)) } }
#[macro_export]
macro_rules! warn { ($($t:tt)*) => { $crate::util::log("WARN", format_args!($($t)*)) } }
#[macro_export]
macro_rules! error { ($($t:tt)*) => { $crate::util::log("ERROR", format_args!($($t)*)) } }

/// Unix time in seconds.
pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

pub fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).expect("OS random source");
    b
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn token() -> String {
    hex(&random::<24>())
}

/// RFC 4122 v4 UUID, used as the stable hardware id we register with Ring/Blink.
pub fn uuid4() -> String {
    let mut b = random::<16>();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = hex(&b);
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

pub fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            _ => o.push(c),
        }
    }
    o
}

pub fn slugify(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let out = out.trim_end_matches('-').to_string();
    if out.is_empty() { "device".into() } else { out }
}

fn local(ts: i64) -> chrono::DateTime<Local> {
    Local.timestamp_opt(ts, 0).single().unwrap_or_else(Local::now)
}

/// "14:02", or "Nov 24, 14:02" when not today.
pub fn fmt_when(ts: i64) -> String {
    let t = local(ts);
    if t.date_naive() == Local::now().date_naive() {
        t.format("%H:%M").to_string()
    } else {
        t.format("%b %-d, %H:%M").to_string()
    }
}

pub fn fmt_clock(ts: i64) -> String {
    local(ts).format("%H:%M").to_string()
}

pub fn fmt_date(ts: i64) -> String {
    local(ts).format("%a %b %-d").to_string()
}

/// Compact duration: "45 s", "12 min", "3 h 5 min", "2 d 4 h".
pub fn fmt_dur(secs: i64) -> String {
    let s = secs.max(0);
    if s < 60 {
        format!("{s} s")
    } else if s < 3600 {
        format!("{} min", s / 60)
    } else if s < 86_400 {
        let m = (s % 3600) / 60;
        if m == 0 { format!("{} h", s / 3600) } else { format!("{} h {} min", s / 3600, m) }
    } else {
        let h = (s % 86_400) / 3600;
        if h == 0 { format!("{} d", s / 86_400) } else { format!("{} d {} h", s / 86_400, h) }
    }
}

pub fn fmt_ago(ts: i64) -> String {
    let d = now() - ts;
    if d < 45 { "just now".into() } else { format!("{} ago", fmt_dur(d)) }
}

pub fn fmt_ms(ms: Option<f64>) -> String {
    match ms {
        None => "—".into(),
        Some(v) if v < 10.0 => format!("{v:.1} ms"),
        Some(v) => format!("{} ms", v.round() as i64),
    }
}

pub fn fmt_bytes(b: u64) -> String {
    let f = b as f64;
    if f >= 1e9 { format!("{:.1} GB", f / 1e9) } else if f >= 1e6 { format!("{:.1} MB", f / 1e6) } else if f >= 1e3 { format!("{:.0} KB", f / 1e3) } else { format!("{b} B") }
}

/// Parses "aa:bb:cc:dd:ee:ff" (also with '-' or no separators).
pub fn parse_mac(s: &str) -> Option<[u8; 6]> {
    let hexs: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if hexs.len() != 12 || s.chars().any(|c| !(c.is_ascii_hexdigit() || c == ':' || c == '-' || c == '.')) {
        return None;
    }
    let mut out = [0u8; 6];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&hexs[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

pub fn fmt_mac(m: &[u8; 6]) -> String {
    m.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(":")
}

/// JSON for embedding inside a <script type="application/json"> tag.
pub fn json_for_script(v: &serde_json::Value) -> String {
    v.to_string().replace('<', "\\u003c").replace('>', "\\u003e").replace('&', "\\u0026")
}
