//! Network helpers: default gateway, TCP checks, Wake-on-LAN, on-demand traceroute.

pub mod icmp;

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::time::Duration;

/// Resolve an IP or hostname to its first IPv4 address.
pub async fn resolve(host: &str) -> Option<Ipv4Addr> {
    if let Ok(ip) = host.parse::<Ipv4Addr>() {
        return Some(ip);
    }
    let addrs = tokio::time::timeout(Duration::from_secs(3), tokio::net::lookup_host((host, 0))).await.ok()?.ok()?;
    for a in addrs {
        if let IpAddr::V4(v4) = a.ip() {
            return Some(v4);
        }
    }
    None
}

pub async fn tcp_check(ip: Ipv4Addr, port: u16, timeout: Duration) -> Option<f64> {
    let start = std::time::Instant::now();
    match tokio::time::timeout(timeout, tokio::net::TcpStream::connect(SocketAddr::new(ip.into(), port))).await {
        Ok(Ok(_)) => Some(start.elapsed().as_secs_f64() * 1000.0),
        _ => None,
    }
}

/// The IPv4 default gateway, or None when the server has no network at all.
#[cfg(target_os = "linux")]
pub fn default_gateway() -> Option<Ipv4Addr> {
    let text = std::fs::read_to_string("/proc/net/route").ok()?;
    let mut best: Option<(u32, Ipv4Addr)> = None;
    for line in text.lines().skip(1) {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 8 || f[1] != "00000000" {
            continue;
        }
        let flags = u32::from_str_radix(f[3], 16).unwrap_or(0);
        if flags & 0x1 == 0 || flags & 0x2 == 0 {
            continue; // not up / not a gateway route
        }
        let gw = u32::from_str_radix(f[2], 16).ok()?;
        let metric = f[6].parse::<u32>().unwrap_or(0);
        let ip = Ipv4Addr::from(gw.to_le_bytes());
        if best.is_none_or(|(m, _)| metric < m) {
            best = Some((metric, ip));
        }
    }
    best.map(|(_, ip)| ip)
}

#[cfg(windows)]
pub fn default_gateway() -> Option<Ipv4Addr> {
    use windows_sys::Win32::NetworkManagement::IpHelper::{GetBestRoute, MIB_IPFORWARDROW};
    unsafe {
        let mut row: MIB_IPFORWARDROW = std::mem::zeroed();
        if GetBestRoute(u32::from_ne_bytes([1, 1, 1, 1]), 0, &mut row) != 0 {
            return None;
        }
        let hop = Ipv4Addr::from(row.dwForwardNextHop.to_ne_bytes());
        (!hop.is_unspecified()).then_some(hop)
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn default_gateway() -> Option<Ipv4Addr> {
    None
}

/// Send a Wake-on-LAN magic packet (three times, to be safe) to the LAN broadcast address.
pub fn wake_on_lan(mac: [u8; 6]) -> std::io::Result<()> {
    let mut pkt = vec![0xffu8; 6];
    for _ in 0..16 {
        pkt.extend_from_slice(&mac);
    }
    let sock = UdpSocket::bind("0.0.0.0:0")?;
    sock.set_broadcast(true)?;
    for _ in 0..3 {
        sock.send_to(&pkt, "255.255.255.255:9")?;
        sock.send_to(&pkt, "255.255.255.255:7")?;
    }
    Ok(())
}

/// Runs the OS traceroute tool, only when the button is pressed. The host always
/// comes from config, and arguments are passed without a shell.
pub async fn traceroute(host: &str) -> String {
    let attempts: Vec<(&str, Vec<&str>)> = if cfg!(windows) {
        vec![("tracert", vec!["-d", "-w", "800", "-h", "20", host])]
    } else {
        vec![("traceroute", vec!["-n", "-q", "1", "-w", "1", "-m", "20", host]), ("tracepath", vec!["-n", "-m", "20", host])]
    };
    for (prog, args) in attempts {
        let mut cmd = tokio::process::Command::new(prog);
        cmd.args(&args).kill_on_drop(true).stdin(std::process::Stdio::null());
        match tokio::time::timeout(Duration::from_secs(90), cmd.output()).await {
            Ok(Ok(out)) => {
                let mut s = String::from_utf8_lossy(&out.stdout).to_string();
                if s.trim().is_empty() {
                    s = String::from_utf8_lossy(&out.stderr).to_string();
                }
                s.truncate(16_000);
                return s;
            }
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Ok(Err(e)) => return format!("{prog} failed: {e}"),
            Err(_) => return format!("{prog} timed out after 90 s"),
        }
    }
    "No traceroute tool found. On Linux install the `traceroute` package (or iputils-tracepath).".into()
}

/// Private, loopback, link-local, CGNAT (Tailscale) and ULA addresses count as LAN.
pub fn is_lan(ip: IpAddr, extra: &[(IpAddr, u8)]) -> bool {
    let ip = match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(IpAddr::V6(v6)),
        v4 => v4,
    };
    let base = match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_private() || v4.is_loopback() || v4.is_link_local() || (o[0] == 100 && (o[1] & 0xc0) == 64)
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            v6.is_loopback() || (s[0] & 0xfe00) == 0xfc00 || (s[0] & 0xffc0) == 0xfe80
        }
    };
    base || extra.iter().any(|(net, bits)| in_cidr(ip, *net, *bits))
}

pub fn parse_cidr(s: &str) -> Option<(IpAddr, u8)> {
    let (ip, bits) = s.split_once('/').unwrap_or((s, if s.contains(':') { "128" } else { "32" }));
    Some((ip.trim().parse().ok()?, bits.trim().parse().ok()?))
}

fn in_cidr(ip: IpAddr, net: IpAddr, bits: u8) -> bool {
    match (ip, net) {
        (IpAddr::V4(a), IpAddr::V4(b)) => {
            let bits = u32::from(bits.min(32));
            let mask = if bits == 0 { 0 } else { u32::MAX << (32 - bits) };
            (u32::from(a) & mask) == (u32::from(b) & mask)
        }
        (IpAddr::V6(a), IpAddr::V6(b)) => {
            let bits = u32::from(bits.min(128));
            let mask = if bits == 0 { 0 } else { u128::MAX << (128 - bits) };
            (u128::from(a) & mask) == (u128::from(b) & mask)
        }
        _ => false,
    }
}
