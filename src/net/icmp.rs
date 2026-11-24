//! ICMP echo without root/admin.
//!
//! * Windows: `IcmpSendEcho` from iphlpapi, which any user may call.
//! * Linux: an unprivileged "ping socket" (SOCK_DGRAM + IPPROTO_ICMP), allowed by
//!   `net.ipv4.ping_group_range`; falls back to a raw socket when the service has
//!   CAP_NET_RAW (the systemd unit grants it).
//!
//! Each ping blocks a pooled thread for at most the timeout, so it runs in
//! `spawn_blocking`.

use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

/// Round-trip time in milliseconds, or None on timeout/error.
pub async fn ping(ip: Ipv4Addr, timeout: Duration) -> Option<f64> {
    tokio::task::spawn_blocking(move || ping_blocking(ip, timeout)).await.ok().flatten()
}

#[cfg(windows)]
pub fn ping_blocking(ip: Ipv4Addr, timeout: Duration) -> Option<f64> {
    use std::ffi::c_void;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::NetworkManagement::IpHelper::{ICMP_ECHO_REPLY, IcmpCloseHandle, IcmpCreateFile, IcmpSendEcho};

    const PAYLOAD: &[u8; 32] = b"umbra-status-check-0123456789abc";
    // u64 storage keeps the reply buffer aligned for ICMP_ECHO_REPLY.
    let reply_len = std::mem::size_of::<ICMP_ECHO_REPLY>() + PAYLOAD.len() + 8 + 64;
    let mut reply = vec![0u64; reply_len.div_ceil(8)];
    let start = Instant::now();
    unsafe {
        let h = IcmpCreateFile();
        if h == INVALID_HANDLE_VALUE || h.is_null() {
            return None;
        }
        let n = IcmpSendEcho(
            h,
            u32::from_ne_bytes(ip.octets()),
            PAYLOAD.as_ptr() as *const c_void,
            PAYLOAD.len() as u16,
            std::ptr::null(),
            reply.as_mut_ptr() as *mut c_void,
            (reply.len() * 8) as u32,
            timeout.as_millis().clamp(100, 10_000) as u32,
        );
        let elapsed = start.elapsed();
        IcmpCloseHandle(h);
        if n == 0 {
            return None;
        }
        let r = &*(reply.as_ptr() as *const ICMP_ECHO_REPLY);
        if r.Status != 0 {
            return None;
        }
        // RoundTripTime has 1 ms resolution; our own timer is finer for LAN pings.
        let measured = elapsed.as_secs_f64() * 1000.0;
        Some(if f64::from(r.RoundTripTime) > measured { f64::from(r.RoundTripTime) } else { measured })
    }
}

#[cfg(not(windows))]
pub fn ping_blocking(ip: Ipv4Addr, timeout: Duration) -> Option<f64> {
    ping_socket(ip, timeout)
}

/// Portable socket implementation (used on Linux; compiles everywhere).
#[cfg_attr(windows, allow(dead_code))]
pub fn ping_socket(ip: Ipv4Addr, timeout: Duration) -> Option<f64> {
    use socket2::{Domain, Protocol, Socket, Type};
    use std::net::{SocketAddr, UdpSocket};

    let (sock, raw) = match Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::ICMPV4)) {
        Ok(s) => (s, false),
        Err(_) => (Socket::new(Domain::IPV4, Type::RAW, Some(Protocol::ICMPV4)).ok()?, true),
    };
    let udp: UdpSocket = sock.into();
    let ident = u16::from_ne_bytes(crate::util::random::<2>());
    let seq = u16::from_ne_bytes(crate::util::random::<2>());
    let mut pkt = [0u8; 40];
    pkt[0] = 8; // echo request
    pkt[4..6].copy_from_slice(&ident.to_be_bytes());
    pkt[6..8].copy_from_slice(&seq.to_be_bytes());
    pkt[8..].copy_from_slice(b"umbra-status-check-0123456789abc");
    let sum = checksum(&pkt);
    pkt[2..4].copy_from_slice(&sum.to_be_bytes());

    let start = Instant::now();
    udp.send_to(&pkt, SocketAddr::new(ip.into(), 0)).ok()?;
    let mut buf = [0u8; 1500];
    loop {
        let left = timeout.checked_sub(start.elapsed())?;
        udp.set_read_timeout(Some(left.max(Duration::from_millis(1)))).ok()?;
        let (n, from) = udp.recv_from(&mut buf).ok()?;
        if from.ip() != std::net::IpAddr::V4(ip) {
            continue;
        }
        let mut b = &buf[..n];
        if raw || (n >= 20 && b[0] >> 4 == 4) {
            let ihl = usize::from(b[0] & 0x0f) * 4;
            if b.len() <= ihl {
                continue;
            }
            b = &b[ihl..];
        }
        // On ping sockets the kernel rewrites the identifier, so match on sequence.
        if b.len() >= 8 && b[0] == 0 && b[6..8] == seq.to_be_bytes() && (!raw || b[4..6] == ident.to_be_bytes()) {
            return Some(start.elapsed().as_secs_f64() * 1000.0);
        }
    }
}

fn checksum(data: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    for chunk in data.chunks(2) {
        let word = if chunk.len() == 2 { u16::from_be_bytes([chunk[0], chunk[1]]) } else { u16::from(chunk[0]) << 8 };
        sum = sum.wrapping_add(u32::from(word));
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}
