//! Single-user login: argon2id password hash, HttpOnly + SameSite=Strict session
//! cookie, and a per-session CSRF token required on every write.

use super::pages::Icons;
use crate::state::{App, Shared};
use crate::util::now;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString, rand_core::OsRng};
use argon2::{Algorithm, Argon2, Params, Version};
use askama::Template;
use axum::extract::{ConnectInfo, Form, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Redirect, Response};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::net::SocketAddr;

pub const COOKIE: &str = "umbra_session";
const SESSION_DAYS: i64 = 30;

#[derive(Clone)]
pub struct Session {
    pub csrf: String,
}

/// OWASP's lower-memory argon2id profile (7 MiB, t=5): cheap enough for an old
/// laptop and it doesn't leave a big allocation behind after each login.
fn argon() -> Argon2<'static> {
    Argon2::new(Algorithm::Argon2id, Version::V0x13, Params::new(7168, 5, 1, None).expect("argon2 params"))
}

pub fn hash_password(pw: &str) -> Result<String, String> {
    let salt = SaltString::generate(&mut OsRng);
    argon().hash_password(pw.as_bytes(), &salt).map(|h| h.to_string()).map_err(|e| e.to_string())
}

fn verify(pw: &str, hash: &str) -> bool {
    PasswordHash::new(hash).is_ok_and(|h| argon().verify_password(pw.as_bytes(), &h).is_ok())
}

pub fn has_password(app: &App) -> bool {
    app.db.meta("password_hash").is_some()
}

fn hash_token(t: &str) -> String {
    crate::util::hex(&Sha256::digest(t.as_bytes()))
}

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers.get_all(header::COOKIE).iter().filter_map(|v| v.to_str().ok()).flat_map(|v| v.split(';')).find_map(|kv| {
        let (k, v) = kv.trim().split_once('=')?;
        (k == name).then(|| v.to_string())
    })
}

fn lookup(app: &App, token: &str) -> Option<String> {
    let hash = hash_token(token);
    let t = now();
    if let Some((csrf, touched)) = app.sessions.lock().ok()?.get(&hash).cloned()
        && t - touched < 600 {
            return Some(csrf);
        }
    let (csrf, last_seen) = app.db.session(&hash)?;
    if t - last_seen > SESSION_DAYS * 86_400 {
        app.db.delete_session(&hash);
        return None;
    }
    app.db.touch_session(&hash);
    app.sessions.lock().ok()?.insert(hash, (csrf.clone(), t));
    Some(csrf)
}

/// Fixed token for `umbra demo`, which has no login and only listens on loopback.
pub const DEMO_CSRF: &str = "demo";

pub async fn require_session(State(app): State<Shared>, mut req: Request, next: Next) -> Response {
    let csrf = if app.demo { Some(DEMO_CSRF.to_string()) } else { cookie_value(req.headers(), COOKIE).and_then(|t| lookup(&app, &t)) };
    let Some(csrf) = csrf else {
        let htmx = req.headers().contains_key("hx-request");
        if htmx || req.uri().path() == "/events" || req.method() != Method::GET {
            let mut r = (StatusCode::UNAUTHORIZED, "Sign in again").into_response();
            r.headers_mut().insert("hx-redirect", HeaderValue::from_static("/login"));
            return r;
        }
        return Redirect::to("/login").into_response();
    };
    if !matches!(*req.method(), Method::GET | Method::HEAD) {
        let sent = req.headers().get("x-csrf-token").and_then(|v| v.to_str().ok()).unwrap_or("");
        if sent.is_empty() || !constant_eq(sent.as_bytes(), csrf.as_bytes()) {
            return (StatusCode::FORBIDDEN, "Missing or wrong CSRF token. Reload the page.").into_response();
        }
    }
    req.extensions_mut().insert(Session { csrf });
    next.run(req).await
}

fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[derive(Template)]
#[template(path = "login.html")]
pub struct LoginPage {
    pub chrome: super::pages::Chrome,
    pub error: Option<String>,
    pub setup_needed: bool,
}
impl Icons for LoginPage {}
impl LoginPage {
    pub fn orb(&self) -> String {
        super::svg::orb(0.22, "ok")
    }
}

pub async fn login_page(State(app): State<Shared>, headers: HeaderMap) -> Response {
    if app.demo || cookie_value(&headers, COOKIE).and_then(|t| lookup(&app, &t)).is_some() {
        return Redirect::to("/").into_response();
    }
    render_login(&app, None, StatusCode::OK)
}

fn render_login(app: &App, error: Option<String>, status: StatusCode) -> Response {
    let page = LoginPage { chrome: super::pages::Chrome::bare(app, "Sign in"), error, setup_needed: !has_password(app) };
    (status, Html(page.render().unwrap_or_default())).into_response()
}

#[derive(Deserialize)]
pub struct LoginForm {
    password: String,
}

pub async fn login_submit(State(app): State<Shared>, ConnectInfo(addr): ConnectInfo<SocketAddr>, Form(f): Form<LoginForm>) -> Response {
    let ip = addr.ip();
    let t = now();
    {
        let mut fails = app.login_fails.lock().unwrap_or_else(|e| e.into_inner());
        fails.retain(|_, (_, at)| t - *at < 600);
        if fails.get(&ip).is_some_and(|(n, _)| *n >= 5) {
            return render_login(&app, Some("Too many attempts. Wait ten minutes and try again.".into()), StatusCode::TOO_MANY_REQUESTS);
        }
    }
    let Some(hash) = app.db.meta("password_hash") else {
        return render_login(&app, None, StatusCode::OK);
    };
    let pw = f.password;
    let ok = tokio::task::spawn_blocking(move || verify(&pw, &hash)).await.unwrap_or(false);
    if !ok {
        {
            let mut fails = app.login_fails.lock().unwrap_or_else(|e| e.into_inner());
            let e = fails.entry(ip).or_insert((0, t));
            e.0 += 1;
            e.1 = t;
        }
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        return render_login(&app, Some("That password didn't match.".into()), StatusCode::UNAUTHORIZED);
    }
    app.login_fails.lock().unwrap_or_else(|e| e.into_inner()).remove(&ip);
    let token = crate::util::token();
    let csrf = crate::util::token();
    app.db.create_session(&hash_token(&token), &csrf);
    let mut r = Redirect::to("/").into_response();
    let cookie = format!("{COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}", SESSION_DAYS * 86_400);
    if let Ok(v) = HeaderValue::from_str(&cookie) {
        r.headers_mut().insert(header::SET_COOKIE, v);
    }
    r
}

pub async fn logout(State(app): State<Shared>, headers: HeaderMap) -> Response {
    if let Some(t) = cookie_value(&headers, COOKIE) {
        let h = hash_token(&t);
        app.db.delete_session(&h);
        app.sessions.lock().unwrap_or_else(|e| e.into_inner()).remove(&h);
    }
    let mut r = StatusCode::NO_CONTENT.into_response();
    r.headers_mut().insert("hx-redirect", HeaderValue::from_static("/login"));
    r.headers_mut().insert(header::SET_COOKIE, HeaderValue::from_static("umbra_session=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0"));
    r
}

/// Used by `umbra set-password` and the settings page.
pub fn set_password(app_db: &crate::db::Db, pw: &str) -> Result<(), String> {
    if pw.chars().count() < 10 {
        return Err("Use at least 10 characters.".into());
    }
    let h = hash_password(pw)?;
    app_db.set_meta("password_hash", &h);
    app_db.delete_all_sessions();
    Ok(())
}

pub fn check_current(app: &App, pw: &str) -> bool {
    app.db.meta("password_hash").is_some_and(|h| verify(pw, &h))
}
