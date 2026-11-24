//! Server-rendered SVG: the eclipse orb, the home map, sparklines and the
//! outage heatmap. No chart or graph library; just strings.

use crate::db::{BUCKETS, Bucket};
use crate::util::esc;
use crate::web::assets::ICONS;
use std::fmt::Write;

/// The eclipse orb: fully lit when all is well; a shadow slides across in
/// proportion to what's wrong, leaving a corona at full eclipse.
pub fn orb(eclipse: f64, tone: &str) -> String {
    let shift = (1.0 - eclipse.clamp(0.0, 1.0)) * 40.0 + 1.5;
    let eclipsed = if eclipse > 0.6 { " orb-eclipsed" } else { "" };
    format!(
        r##"<svg class="orb orb-{tone}{eclipsed}" viewBox="0 0 64 64" role="img" aria-label="Status: {tone}">
<defs>
<radialGradient id="orb-core" cx="40%" cy="36%" r="72%"><stop offset="0" style="stop-color:var(--orb-hi)"/><stop offset=".6" style="stop-color:var(--orb)"/><stop offset="1" style="stop-color:var(--orb-lo)"/></radialGradient>
<radialGradient id="orb-halo"><stop offset=".5" style="stop-color:var(--orb);stop-opacity:.42"/><stop offset="1" style="stop-color:var(--orb);stop-opacity:0"/></radialGradient>
<clipPath id="orb-clip"><circle cx="32" cy="32" r="16"/></clipPath>
</defs>
<circle class="orb-halo" cx="32" cy="32" r="31" fill="url(#orb-halo)"/>
<circle class="orb-body" cx="32" cy="32" r="16" fill="url(#orb-core)"/>
<g clip-path="url(#orb-clip)"><circle class="orb-shadow" cx="32" cy="32" r="16.6" style="transform:translateX({shift:.1}px)"/></g>
<circle class="orb-rim" cx="32" cy="32" r="16.4"/>
</svg>"##
    )
}

pub struct MapNode {
    pub id: String,
    pub name: String,
    pub icon: String,
    pub status: &'static str,
    pub latency: String,
    pub href: String,
}

const W: f64 = 960.0;
const H: f64 = 560.0;
const CX: f64 = 480.0;
const CY: f64 = 300.0;

fn node(out: &mut String, n: &MapNode, x: f64, y: f64, r: f64, cls: &str) {
    let icon = r * 0.95;
    let _ = write!(
        out,
        r##"<g class="node node-{st} {cls}" id="node-{id}" transform="translate({x:.1} {y:.1})"><a href="{href}" aria-label="{name}"><circle class="node-halo" r="{halo:.1}"/><circle class="node-bg" r="{r:.1}"/><circle class="node-ring" r="{r:.1}"/><use href="{ICONS}#{ic}" x="{o:.1}" y="{o:.1}" width="{icon:.1}" height="{icon:.1}" class="node-icon"/></a></g>"##,
        st = n.status,
        id = esc(&n.id),
        href = esc(&n.href),
        name = esc(&n.name),
        halo = r + 9.0,
        ic = esc(&n.icon),
        o = -icon / 2.0,
    );
}

fn label(out: &mut String, n: &MapNode, x: f64, y: f64, anchor: &str) {
    let _ = write!(
        out,
        r#"<text class="node-label" x="{x:.1}" y="{y:.1}" text-anchor="{anchor}">{}</text><text class="node-sub mono" x="{x:.1}" y="{:.1}" text-anchor="{anchor}" data-lat="{}">{}</text>"#,
        esc(&n.name),
        y + 15.0,
        esc(&n.id),
        esc(&n.latency)
    );
}

/// Radial home map: the internet at the top, the router in the middle, devices
/// on an orbit around it, joined by thin curved links tinted by status.
pub fn map(internet: &MapNode, router: &MapNode, devices: &[MapNode]) -> String {
    let mut s = String::with_capacity(6000 + devices.len() * 900);
    let _ = write!(
        s,
        r##"<svg class="map" viewBox="0 0 {W} {H}" role="img" aria-label="Home network map">
<defs><radialGradient id="map-glow"><stop offset="0" style="stop-color:var(--accent);stop-opacity:.16"/><stop offset="1" style="stop-color:var(--accent);stop-opacity:0"/></radialGradient></defs>
<circle cx="{CX}" cy="{CY}" r="210" fill="url(#map-glow)"/>
<ellipse class="orbit" cx="{CX}" cy="{CY}" rx="190" ry="105"/>
<ellipse class="orbit orbit-outer" cx="{CX}" cy="{CY}" rx="365" ry="205"/>
<g class="links">"##
    );
    let ix = CX;
    let iy = 64.0;
    let _ = write!(s, r#"<path id="link-internet" class="link link-{}" d="M{CX},{CY} C{},{} {},{} {ix},{iy}"/>"#, internet.status, CX - 40.0, CY - 90.0, CX + 40.0, iy + 90.0);

    let n = devices.len();
    let mut placed = vec![];
    if n == 0 {
        let (x, y) = (CX, CY + 205.0);
        let _ = write!(s, r#"<path class="link link-ghost" d="M{CX},{CY} Q{},{} {x},{y}"/>"#, CX + 30.0, CY + 100.0);
        placed.push((x, y, 90.0f64));
    } else {
        let span = (60.0 * (n as f64 - 1.0)).min(250.0);
        let a0 = 90.0 - span / 2.0;
        for (i, d) in devices.iter().enumerate() {
            let a = if n == 1 { 90.0 } else { a0 + span * i as f64 / (n as f64 - 1.0) };
            let rad = a.to_radians();
            let (x, y) = (CX + 365.0 * rad.cos(), CY + 205.0 * rad.sin());
            // Control point: midpoint pushed sideways, alternating, for a soft curve.
            let (mx, my) = ((CX + x) / 2.0, (CY + y) / 2.0);
            let (dx, dy) = (x - CX, y - CY);
            let len = (dx * dx + dy * dy).sqrt().max(1.0);
            let side = if i % 2 == 0 { 1.0 } else { -1.0 };
            let (qx, qy) = (mx - dy / len * len * 0.14 * side, my + dx / len * len * 0.14 * side);
            let _ = write!(s, r#"<path id="link-{}" class="link link-{}" d="M{CX},{CY} Q{qx:.1},{qy:.1} {x:.1},{y:.1}"/>"#, esc(&d.id), d.status);
            placed.push((x, y, a));
        }
    }
    s.push_str(r#"</g><g class="pulses"></g><g class="nodes">"#);
    node(&mut s, internet, ix, iy, 19.0, "node-internet");
    label(&mut s, internet, ix + 34.0, iy - 2.0, "start");
    node(&mut s, router, CX, CY, 27.0, "node-router");
    label(&mut s, router, CX, CY + 50.0, "middle");
    if n == 0 {
        let (x, y, _) = placed[0];
        let _ = write!(s, r##"<g class="node node-ghost" transform="translate({x} {y})"><a href="/settings#add-device"><circle class="node-bg" r="18"/><circle class="node-ring" r="18"/><use href="{ICONS}#plus" x="-9" y="-9" width="18" height="18" class="node-icon"/></a></g><text class="node-label" x="{x}" y="{}" text-anchor="middle">Add your first device</text>"##, y + 38.0);
    }
    for (d, (x, y, a)) in devices.iter().zip(&placed) {
        node(&mut s, d, *x, *y, 17.0, "");
        let rad = a.to_radians();
        let (c, sn) = (rad.cos(), rad.sin());
        let (anchor, lx, ly) = if c > 0.42 {
            ("start", x + 28.0, y + 4.0)
        } else if c < -0.42 {
            ("end", x - 28.0, y + 4.0)
        } else if sn > 0.0 {
            ("middle", *x, y + 38.0)
        } else {
            ("middle", *x, y - 42.0)
        };
        label(&mut s, d, lx, ly, anchor);
    }
    s.push_str("</g></svg>");
    s
}

/// 24 h latency line; red ticks mark 15-minute buckets that were mostly down.
pub fn sparkline(b: &[Bucket; BUCKETS], height: f64, cls: &str) -> String {
    let lats: Vec<f64> = b.iter().filter_map(|x| x.lat).collect();
    let mut s = format!(r#"<svg class="spark {cls}" viewBox="0 0 {BUCKETS} {height}" preserveAspectRatio="none" aria-hidden="true">"#);
    if lats.is_empty() {
        let _ = write!(s, r#"<line class="spark-empty" x1="0" x2="{BUCKETS}" y1="{y}" y2="{y}"/></svg>"#, y = height - 2.0);
        return s;
    }
    let min = lats.iter().cloned().fold(f64::INFINITY, f64::min);
    let max = lats.iter().cloned().fold(0.0, f64::max).max(min + 1.0);
    let top = 3.0;
    let usable = height - 7.0;
    let y = |v: f64| top + (1.0 - ((v - min) / (max - min))) * usable;
    let mut line = String::new();
    let mut area = String::new();
    let mut run: Vec<(f64, f64)> = vec![];
    let flush = |run: &mut Vec<(f64, f64)>, line: &mut String, area: &mut String| {
        if run.is_empty() {
            return;
        }
        if run.len() == 1 {
            let (x, yy) = run[0];
            run.insert(0, (x - 0.5, yy));
            run[1].0 = x + 0.5;
        }
        let mut d = String::new();
        for (i, (x, yy)) in run.iter().enumerate() {
            let _ = write!(d, "{}{x:.1},{yy:.1}", if i == 0 { "M" } else { "L" });
        }
        let _ = write!(line, r#"<path class="spark-line" d="{d}"/>"#);
        let (fx, _) = run[0];
        let (lx, _) = run[run.len() - 1];
        let _ = write!(area, r#"<path class="spark-area" d="{d}L{lx:.1},{height}L{fx:.1},{height}Z"/>"#);
        run.clear();
    };
    for (i, bk) in b.iter().enumerate() {
        match bk.lat {
            Some(v) => run.push((i as f64 + 0.5, y(v))),
            None => flush(&mut run, &mut line, &mut area),
        }
    }
    flush(&mut run, &mut line, &mut area);
    s.push_str(&area);
    s.push_str(&line);
    for (i, bk) in b.iter().enumerate() {
        if bk.uptime().is_some_and(|u| u < 0.5) {
            let _ = write!(s, r#"<rect class="spark-down" x="{i}" y="{}" width="1" height="3"/>"#, height - 3.0);
        }
    }
    s.push_str("</svg>");
    s
}

fn heat_class(b: &Bucket) -> &'static str {
    match b.uptime() {
        None => "h-none",
        Some(u) if u >= 0.995 => "h-up",
        Some(u) if u >= 0.9 => "h-dip",
        Some(u) if u >= 0.5 => "h-bad",
        Some(_) => "h-down",
    }
}

/// One heatmap row: 96 cells of 15 minutes, consecutive equal cells merged.
pub fn heat_strip(b: &[Bucket; BUCKETS]) -> String {
    let mut s = format!(r#"<svg class="heat" viewBox="0 0 {BUCKETS} 1" preserveAspectRatio="none" aria-hidden="true">"#);
    let mut i = 0;
    while i < BUCKETS {
        let c = heat_class(&b[i]);
        let mut j = i + 1;
        while j < BUCKETS && heat_class(&b[j]) == c {
            j += 1;
        }
        let _ = write!(s, r#"<rect class="{c}" x="{i}" width="{}" height="1"/>"#, j - i);
        i = j;
    }
    s.push_str(r#"<rect width="96" height="1" fill="url(#heat-gap)"/></svg>"#);
    s
}

/// Shared <defs> used by every heat strip (cell separators).
pub const HEAT_DEFS: &str = r#"<svg width="0" height="0" style="position:absolute" aria-hidden="true"><defs><pattern id="heat-gap" width="1" height="1" patternUnits="userSpaceOnUse"><rect x=".86" width=".14" height="1" class="heat-gap"/></pattern></defs></svg>"#;
