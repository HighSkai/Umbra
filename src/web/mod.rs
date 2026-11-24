//! HTTP server: server-rendered HTML + htmx, a tiny SSE stream for live updates.

pub mod assets;
pub mod auth;
pub mod events;
pub mod live_routes;
pub mod pages;
pub mod settings;
pub mod svg;

use crate::state::Shared;
use axum::Router;
use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use std::io::Write;
use std::net::SocketAddr;
use std::sync::atomic::Ordering;

pub fn router(app: Shared) -> Router {
    let protected = Router::new()
        .route("/", get(pages::dashboard))
        .route("/partials/header", get(pages::header_partial))
        .route("/partials/live", get(pages::live_partial))
        .route("/device/{id}", get(pages::device_page))
        .route("/device/{id}/wake", post(pages::wake))
        .route("/device/{id}/traceroute", post(pages::traceroute))
        .route("/settings", get(settings::page))
        .route("/settings/device", post(settings::save_device))
        .route("/settings/device/{id}/delete", post(settings::delete_device))
        .route("/settings/network", post(settings::save_network))
        .route("/settings/alerts", post(settings::save_alerts))
        .route("/settings/alerts/test", post(settings::test_alerts))
        .route("/settings/password", post(settings::change_password))
        .route("/settings/integration/{name}/login", post(settings::integ_login))
        .route("/settings/integration/{name}/code", post(settings::integ_code))
        .route("/settings/integration/{name}/disconnect", post(settings::integ_disconnect))
        .route("/settings/integration/{name}/poll", post(settings::integ_poll))
        .route("/cameras/blink/{id}/snapshot", post(pages::blink_snapshot))
        .route("/cameras/{source}/{id}/snapshot.jpg", get(pages::snapshot_image))
        .route("/live/{source}/{id}/start", post(live_routes::start))
        .route("/live/s/{sid}/offer", post(live_routes::offer))
        .route("/live/s/{sid}/candidate", post(live_routes::candidate))
        .route("/live/s/{sid}/keepalive", post(live_routes::keepalive))
        .route("/live/s/{sid}/stop", post(live_routes::stop))
        .route("/live/s/{sid}/events", get(live_routes::events))
        .route("/live/s/{sid}/stream.mp4", get(live_routes::stream))
        .route("/events", get(events::sse))
        .route("/debug/stats", get(pages::stats_page))
        .route("/debug/stats.json", get(pages::stats_json))
        .route("/logout", post(auth::logout))
        .layer(middleware::from_fn_with_state(app.clone(), auth::require_session));

    Router::new()
        .merge(protected)
        .route("/login", get(auth::login_page).post(auth::login_submit))
        .route("/assets/{file}", get(assets::serve))
        .fallback(|| async { (StatusCode::NOT_FOUND, "Not found") })
        .layer(middleware::from_fn_with_state(app.clone(), guard))
        .with_state(app)
}

const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; \
media-src 'self' blob:; font-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'";

/// Outermost layer: LAN-only, same-origin writes, security headers, gzip for HTML.
async fn guard(State(app): State<Shared>, ConnectInfo(addr): ConnectInfo<SocketAddr>, req: Request, next: Next) -> Response {
    app.stats.requests.fetch_add(1, Ordering::Relaxed);
    let extra: Vec<_> = app.cfg().server.allow_cidrs.iter().filter_map(|c| crate::net::parse_cidr(c)).collect();
    if !crate::net::is_lan(addr.ip(), &extra) {
        return (StatusCode::FORBIDDEN, "Umbra only answers on your home network.").into_response();
    }
    if !matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS) {
        let host = req.headers().get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("");
        let origin = req.headers().get(header::ORIGIN).or_else(|| req.headers().get(header::REFERER)).and_then(|h| h.to_str().ok());
        match origin {
            Some(o) if origin_host(o) != host => return (StatusCode::FORBIDDEN, "Cross-site request refused.").into_response(),
            None if req.uri().path() == "/login" => return (StatusCode::FORBIDDEN, "Missing Origin header.").into_response(),
            _ => {}
        }
    }
    let gzip_ok = req.headers().get(header::ACCEPT_ENCODING).and_then(|v| v.to_str().ok()).is_some_and(|v| v.contains("gzip"));
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    h.insert("referrer-policy", HeaderValue::from_static("same-origin"));
    h.insert("x-frame-options", HeaderValue::from_static("DENY"));
    h.insert("permissions-policy", HeaderValue::from_static("camera=(), microphone=(), geolocation=()"));
    h.insert("content-security-policy", HeaderValue::from_static(CSP));
    let is_html = h.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|v| v.starts_with("text/html"));
    if is_html && !h.contains_key(header::CACHE_CONTROL) {
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    if is_html && gzip_ok && !h.contains_key(header::CONTENT_ENCODING) {
        return gzip_response(resp).await;
    }
    resp
}

fn origin_host(o: &str) -> &str {
    let rest = o.split_once("://").map(|(_, r)| r).unwrap_or(o);
    rest.split('/').next().unwrap_or("")
}

async fn gzip_response(resp: Response) -> Response {
    let (mut parts, body) = resp.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, 8 * 1024 * 1024).await else {
        return Response::from_parts(parts, Body::empty());
    };
    if bytes.len() < 1024 {
        return Response::from_parts(parts, Body::from(bytes));
    }
    let mut enc = flate2::write::GzEncoder::new(Vec::with_capacity(bytes.len() / 4), flate2::Compression::new(6));
    if enc.write_all(&bytes).is_err() {
        return Response::from_parts(parts, Body::from(bytes));
    }
    let Ok(gz) = enc.finish() else { return Response::from_parts(parts, Body::from(bytes)) };
    parts.headers.remove(header::CONTENT_LENGTH);
    parts.headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
    parts.headers.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    Response::from_parts(parts, Body::from(gz))
}

pub async fn serve(app: Shared) -> Result<(), String> {
    let bind = app.cfg().server.bind.clone();
    let listener = tokio::net::TcpListener::bind(&bind).await.map_err(|e| format!("can't listen on {bind}: {e}"))?;
    crate::info!("listening on http://{bind}");
    let app2 = app.clone();
    let shutdown = async move {
        #[cfg(unix)]
        {
            let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
        crate::info!("shutting down");
        app2.flush_checks();
        // Open SSE streams never finish on their own; don't wait on them.
        tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            std::process::exit(0);
        });
    };
    axum::serve(listener, router(app.clone()).into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(shutdown)
        .await
        .map_err(|e| e.to_string())
}
