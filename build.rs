//! Bundles, hashes and precompresses everything in `web/` so the binary can
//! serve it with immutable cache headers. No JS build step: CSS files are
//! concatenated, JS is concatenated, Lucide icons become one SVG sprite.

use flate2::{Compression, write::GzEncoder};
use std::{
    env, fs,
    io::Write,
    path::{Path, PathBuf},
};

struct Out {
    konst: &'static str,
    url: String,
    file: String,
    mime: &'static str,
    gz: bool,
}

fn fnv(data: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in data {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")[..10].to_string()
}

fn emit(out_dir: &Path, list: &mut Vec<Out>, konst: &'static str, stem: &str, ext: &str, mime: &'static str, data: Vec<u8>, gz: bool) -> String {
    let file = format!("{stem}.{}.{ext}", fnv(&data));
    fs::write(out_dir.join(&file), &data).unwrap();
    if gz {
        let mut enc = GzEncoder::new(Vec::new(), Compression::best());
        enc.write_all(&data).unwrap();
        fs::write(out_dir.join(format!("{file}.gz")), enc.finish().unwrap()).unwrap();
    }
    let url = format!("/assets/{file}");
    list.push(Out { konst, url: url.clone(), file, mime, gz });
    url
}

/// Strips /* comments */ and blank indentation. Good enough for hand-written CSS.
fn css_min(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut rest = src;
    while let Some(start) = rest.find("/*") {
        out.push_str(&rest[..start]);
        match rest[start..].find("*/") {
            Some(end) => rest = &rest[start + end + 2..],
            None => {
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join("\n")
}

fn read(p: PathBuf) -> Vec<u8> {
    fs::read(&p).unwrap_or_else(|e| panic!("reading {}: {e}", p.display()))
}

fn main() {
    let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let web = root.join("web");
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    println!("cargo:rerun-if-changed=web");
    println!("cargo:rerun-if-changed=templates");

    let mut list = Vec::new();

    let font = emit(&out_dir, &mut list, "FONT_MONO", "geist-mono", "woff2", "font/woff2", read(web.join("fonts/geist-mono-latin-wght.woff2")), false);

    // Icon sprite: every web/icons/*.svg becomes a <symbol id="name">.
    let mut names: Vec<String> = Vec::new();
    let mut sprite = String::from(r#"<svg xmlns="http://www.w3.org/2000/svg">"#);
    let mut entries: Vec<_> = fs::read_dir(web.join("icons")).unwrap().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "svg")).collect();
    entries.sort();
    for p in entries {
        let name = p.file_stem().unwrap().to_string_lossy().to_string();
        let src = String::from_utf8(read(p)).unwrap();
        let open = src.find("<svg").expect("svg tag");
        let body_start = open + src[open..].find('>').unwrap() + 1;
        let body_end = src.rfind("</svg>").unwrap();
        let body: String = src[body_start..body_end].lines().map(str::trim).collect::<Vec<_>>().join("");
        sprite.push_str(&format!(r#"<symbol id="{name}" viewBox="0 0 24 24">{body}</symbol>"#));
        names.push(name);
    }
    sprite.push_str("</svg>");
    let icons = emit(&out_dir, &mut list, "ICONS", "icons", "svg", "image/svg+xml", sprite.into_bytes(), true);

    let mut css = String::new();
    for f in ["tokens.css", "base.css", "layout.css", "components.css", "map.css"] {
        css.push_str(&String::from_utf8(read(web.join("css").join(f))).unwrap());
        css.push('\n');
    }
    let css = css_min(&css).replace("__FONT_MONO__", &font).replace("__ICONS__", &icons);
    emit(&out_dir, &mut list, "CSS", "app", "css", "text/css; charset=utf-8", css.into_bytes(), true);

    let mut js = read(web.join("vendor/htmx-2.0.11.min.js"));
    js.extend_from_slice(b"\n;\n");
    js.extend(read(web.join("js/app.js")));
    emit(&out_dir, &mut list, "JS", "app", "js", "text/javascript; charset=utf-8", js, true);
    emit(&out_dir, &mut list, "LIVE_JS", "live", "js", "text/javascript; charset=utf-8", read(web.join("js/live.js")), true);
    emit(&out_dir, &mut list, "FAVICON", "favicon", "svg", "image/svg+xml", read(web.join("favicon.svg")), true);

    let mut rs = String::new();
    for o in &list {
        rs.push_str(&format!("pub const {}: &str = {:?};\n", o.konst, o.url));
    }
    rs.push_str("pub static ASSETS: &[Asset] = &[\n");
    for o in &list {
        let gz = if o.gz { format!("Some(include_bytes!(concat!(env!(\"OUT_DIR\"), \"/{}.gz\")))", o.file) } else { "None".into() };
        rs.push_str(&format!(
            "    Asset {{ url: {:?}, mime: {:?}, raw: include_bytes!(concat!(env!(\"OUT_DIR\"), \"/{}\")), gz: {} }},\n",
            o.url, o.mime, o.file, gz
        ));
    }
    rs.push_str("];\n");
    rs.push_str(&format!("pub const ICON_NAMES: &[&str] = &{names:?};\n"));
    fs::write(out_dir.join("assets.rs"), rs).unwrap();
}
