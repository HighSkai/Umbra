//! Unit tests for the parts that decide what you see: the status model,
//! the header verdict, LAN detection, parsing and the SVG builders.

use crate::config::{Config, Device, Kind};
use crate::db::{BUCKETS, Bucket};
use crate::state::{Status, Target, TargetKind, overview};
use crate::util;
use std::net::IpAddr;

fn device(id: &str, down_after: u32) -> Target {
    Target::new(id, id, TargetKind::Device(Kind::Computer), "10.0.0.2".into(), "laptop", down_after, 150.0, true)
}

#[test]
fn goes_down_only_after_n_failures_and_comes_back() {
    let mut t = device("pc", 3);
    assert_eq!(t.apply(true, Some(2.0), 100, 30.0).new, Status::Up);
    assert_eq!(t.apply(false, None, 110, 30.0).new, Status::Up, "one miss is not down");
    assert_eq!(t.apply(false, None, 120, 30.0).new, Status::Up, "too few samples to call it lossy");
    let a = t.apply(false, None, 130, 30.0);
    assert_eq!(a.new, Status::Down);
    assert_eq!(t.since, 110, "outage starts at the first failed check");
    let back = t.apply(true, Some(3.0), 200, 30.0);
    assert_eq!(back.prev, Status::Down);
    assert_ne!(back.new, Status::Down);
}

#[test]
fn sustained_loss_is_degraded() {
    let mut t = device("cam", 5);
    let pattern = [true, false, true, false, true, false, true];
    let mut last = Status::Unknown;
    for (i, ok) in pattern.iter().enumerate() {
        last = t.apply(*ok, ok.then_some(3.0), i as i64, 30.0).new;
    }
    assert_eq!(last, Status::Degraded, "3 of 7 lost is a flaky link");
}

#[test]
fn high_latency_is_degraded() {
    let mut t = device("tv", 3);
    assert_eq!(t.apply(true, Some(400.0), 1, 30.0).new, Status::Degraded);
    assert_eq!(t.apply(true, Some(4.0), 2, 30.0).new, Status::Up);
}

#[test]
fn unknown_stays_unknown_until_confirmed() {
    let mut t = device("nas", 3);
    assert_eq!(t.apply(false, None, 1, 30.0).new, Status::Unknown);
    assert_eq!(t.apply(false, None, 2, 30.0).new, Status::Unknown);
    assert_eq!(t.apply(false, None, 3, 30.0).new, Status::Down);
}

fn net(id: &str, kind: TargetKind, status: Status) -> Target {
    let mut t = Target::new(id, id, kind, "x".into(), "globe", 3, 150.0, true);
    t.status = status;
    t
}

#[test]
fn verdict_separates_internet_router_and_power() {
    let mut list = vec![net("router", TargetKind::Router, Status::Up), net("internet", TargetKind::Internet, Status::Up), device("pc", 3)];
    list[2].status = Status::Up;
    assert_eq!(overview(&list, false, None).verdict, "All clear");

    list[1].status = Status::Down;
    assert_eq!(overview(&list, false, None).verdict, "Internet down");

    list[0].status = Status::Down;
    assert_eq!(overview(&list, false, None).verdict, "Router down");

    let ov = overview(&list, true, Some(42.0));
    assert!(ov.verdict.starts_with("Power outage"), "{}", ov.verdict);
    assert!(ov.eclipse > 0.5);

    list[0].status = Status::Up;
    list[1].status = Status::Up;
    list[2].status = Status::Down;
    let ov = overview(&list, false, None);
    assert_eq!(ov.verdict, "1 device down");
    assert_eq!(ov.mood, "alert");
}

#[test]
fn lan_detection() {
    let ip = |s: &str| s.parse::<IpAddr>().unwrap();
    for ok in ["192.168.1.20", "10.1.2.3", "172.16.5.4", "127.0.0.1", "100.101.102.103", "fd12::1", "fe80::1", "::1", "::ffff:192.168.1.9"] {
        assert!(crate::net::is_lan(ip(ok), &[]), "{ok} should be LAN");
    }
    for bad in ["8.8.8.8", "1.1.1.1", "2606:4700::1111", "100.128.0.1"] {
        assert!(!crate::net::is_lan(ip(bad), &[]), "{bad} should not be LAN");
    }
    let extra = vec![crate::net::parse_cidr("203.0.113.0/24").unwrap()];
    assert!(crate::net::is_lan(ip("203.0.113.7"), &extra));
}

#[test]
fn mac_parsing() {
    assert_eq!(util::parse_mac("aa:bb:cc:dd:ee:ff"), Some([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]));
    assert_eq!(util::parse_mac("AA-BB-CC-DD-EE-01").map(|m| util::fmt_mac(&m)).as_deref(), Some("AA:BB:CC:DD:EE:01"));
    assert_eq!(util::parse_mac("aabbccddeeff").is_some(), true);
    assert!(util::parse_mac("aa:bb:cc").is_none());
    assert!(util::parse_mac("zz:bb:cc:dd:ee:ff").is_none());
}

#[test]
fn small_formatters() {
    assert_eq!(util::fmt_dur(42), "42 s");
    assert_eq!(util::fmt_dur(18 * 60), "18 min");
    assert_eq!(util::fmt_dur(3 * 3600 + 5 * 60), "3 h 5 min");
    assert_eq!(util::fmt_dur(2 * 86_400 + 4 * 3600), "2 d 4 h");
    assert_eq!(util::slugify("Living room TV!"), "living-room-tv");
    let u = util::uuid4();
    assert_eq!(u.len(), 36);
    assert_eq!(&u[14..15], "4");
    assert!(util::json_for_script(&serde_json::json!({"a": "</script>"})).contains("\\u003c/script\\u003e"));
}

#[test]
fn config_validation_and_roundtrip() {
    let mut c = Config::default();
    c.devices.push(Device { id: "pc".into(), name: "PC".into(), host: "10.0.0.2".into(), ping: true, ..Default::default() });
    assert!(c.validate().is_ok());
    let text = toml::to_string_pretty(&c).unwrap();
    let back: Config = toml::from_str(&text).unwrap();
    assert_eq!(back.devices.len(), 1);
    assert_eq!(back.network.internet_targets, vec!["1.1.1.1", "8.8.8.8"]);

    c.devices.push(Device { id: "pc".into(), name: "Dup".into(), host: "10.0.0.3".into(), ..Default::default() });
    assert!(c.validate().is_err(), "duplicate ids are rejected");
    c.devices.pop();
    c.devices.push(Device { id: "router".into(), name: "R".into(), host: "10.0.0.1".into(), ..Default::default() });
    assert!(c.validate().is_err(), "reserved ids are rejected");
    c.devices.pop();
    assert_eq!(c.unique_id("PC"), "pc-2");
}

#[test]
fn heatmap_merges_equal_runs() {
    let mut b = [Bucket { n: 4, ok: 4, lat: Some(2.0) }; BUCKETS];
    b[10] = Bucket { n: 4, ok: 0, lat: None };
    let s = crate::web::svg::heat_strip(&b);
    // up run, one down cell, up run, plus the gap overlay
    assert_eq!(s.matches("<rect").count(), 4, "{s}");
    assert!(s.contains("h-down"));
}

#[test]
fn sparkline_handles_gaps_and_empty() {
    let empty = [Bucket::default(); BUCKETS];
    assert!(crate::web::svg::sparkline(&empty, 24.0, "").contains("spark-empty"));
    let mut b = [Bucket::default(); BUCKETS];
    for (i, x) in b.iter_mut().enumerate().take(40) {
        *x = Bucket { n: 2, ok: 2, lat: Some(1.0 + i as f64) };
    }
    b[60] = Bucket { n: 2, ok: 0, lat: None };
    let s = crate::web::svg::sparkline(&b, 24.0, "");
    assert_eq!(s.matches("spark-line").count(), 1);
    assert!(s.contains("spark-down"));
}

#[test]
fn orb_shadow_moves_with_eclipse() {
    let lit = crate::web::svg::orb(0.0, "ok");
    let dark = crate::web::svg::orb(1.0, "bad");
    assert!(lit.contains("translateX(41.5px)"));
    assert!(dark.contains("translateX(1.5px)") && dark.contains("orb-eclipsed"));
}
