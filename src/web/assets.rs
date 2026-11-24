//! Static assets embedded at build time (see build.rs), served with hashed names.

use axum::extract::Path;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

pub struct Asset {
    pub url: &'static str,
    pub mime: &'static str,
    pub raw: &'static [u8],
    pub gz: Option<&'static [u8]>,
}

include!(concat!(env!("OUT_DIR"), "/assets.rs"));

/// Inline reference to an icon in the sprite.
pub fn icon(name: &str) -> String {
    format!(r##"<svg class="icon" aria-hidden="true" focusable="false"><use href="{ICONS}#{name}"></use></svg>"##)
}

pub async fn serve(Path(file): Path<String>, headers: HeaderMap) -> Response {
    let url = format!("/assets/{file}");
    let Some(a) = ASSETS.iter().find(|a| a.url == url) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let gzip_ok = headers.get(header::ACCEPT_ENCODING).and_then(|v| v.to_str().ok()).is_some_and(|v| v.contains("gzip"));
    let mut resp = match (a.gz, gzip_ok) {
        (Some(gz), true) => {
            let mut r = gz.into_response();
            r.headers_mut().insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
            r
        }
        _ => a.raw.into_response(),
    };
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(a.mime));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=31536000, immutable"));
    h.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    resp
}
