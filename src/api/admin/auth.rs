//! Admin sign-in: password check, sessions, the session cookie and the CSRF guard.

use std::{
    net::{IpAddr, SocketAddr},
    time::{Duration, Instant},
};

use axum::{
    extract::{FromRequestParts, Request, State},
    http::{header, request::Parts, HeaderMap, HeaderValue, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use dashmap::DashMap;
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use super::{AdminState, SESSION_TTL};
use crate::api::{
    auth::PeerAddr,
    error::{ApiError, Body},
    session::hash_token,
};

pub const COOKIE: &str = "lynx_admin";
/// Header every mutating request must carry (a cross-site form or image cannot set it, and a
/// cross-origin script would need a CORS preflight that this server never grants).
pub const CSRF_HEADER: &str = "x-lynx-admin";
/// Wait before answering a wrong password.
const FAIL_DELAY: Duration = Duration::from_millis(300);
const MAX_PASSWORD_BYTES: usize = 1024;

pub fn digest(s: &str) -> [u8; 32] {
    Sha256::digest(s.as_bytes()).into()
}

/// Constant-time comparison: the time taken never depends on where the inputs differ.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let diff = a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y));
    std::hint::black_box(diff) == 0
}

/// In-memory admin sessions (token hash -> expiry). A restart signs everyone out.
pub struct AdminSessions {
    map: DashMap<String, Instant>,
    ttl: Duration,
    max: usize,
}

impl AdminSessions {
    pub fn new(ttl: Duration, max: usize) -> Self {
        Self {
            map: DashMap::new(),
            ttl,
            max: max.max(1),
        }
    }

    pub fn create(&self) -> String {
        self.sweep();
        if self.map.len() >= self.max {
            // Full of live sessions: drop the oldest one.
            let oldest = self.map.iter().min_by_key(|e| *e.value()).map(|e| e.key().clone());
            if let Some(k) = oldest {
                self.map.remove(&k);
            }
        }
        let mut buf = [0u8; 32];
        getrandom::fill(&mut buf).expect("OS random source");
        let token: String = buf.iter().map(|b| format!("{b:02x}")).collect();
        self.map.insert(hash_token(&token), Instant::now() + self.ttl);
        token
    }

    pub fn valid(&self, token: &str) -> bool {
        if token.len() != 64 {
            return false;
        }
        let hash = hash_token(token);
        match self.map.get(&hash).map(|e| *e.value()) {
            Some(exp) if Instant::now() < exp => true,
            Some(_) => {
                self.map.remove(&hash);
                false
            }
            None => false,
        }
    }

    pub fn remove(&self, token: &str) {
        self.map.remove(&hash_token(token));
    }

    pub fn sweep(&self) {
        let now = Instant::now();
        self.map.retain(|_, exp| now < *exp);
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.map.len()
    }
}

/// Client address. Behind nginx (a loopback peer) it is nginx's `X-Real-IP`; otherwise the socket.
/// `X-Forwarded-For` is never trusted here.
pub fn client_ip(headers: &HeaderMap, peer: Option<SocketAddr>) -> String {
    let real_ip = || {
        headers
            .get("x-real-ip")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<IpAddr>().ok())
    };
    match peer {
        Some(p) if p.ip().is_loopback() => real_ip().unwrap_or(p.ip()).to_string(),
        Some(p) => p.ip().to_string(),
        None => real_ip().map(|ip| ip.to_string()).unwrap_or_else(|| "unknown".into()),
    }
}

fn cookie_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == COOKIE)
        .map(|(_, v)| v.trim().to_owned())
}

/// Over HTTPS (a TLS proxy in front of nginx says so with `X-Forwarded-Proto`) or forced by
/// `ADMIN_COOKIE_SECURE`.
fn secure(st: &AdminState, headers: &HeaderMap) -> bool {
    st.cookie_secure.unwrap_or_else(|| {
        headers
            .get("x-forwarded-proto")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.split(',').next().unwrap_or("").trim().eq_ignore_ascii_case("https"))
    })
}

pub fn session_cookie(token: &str, secure: bool) -> String {
    format!(
        "{COOKIE}={token}; Path=/; Max-Age={}; HttpOnly; SameSite=Strict{}",
        SESSION_TTL.as_secs(),
        if secure { "; Secure" } else { "" }
    )
}

fn clear_cookie(secure: bool) -> String {
    format!(
        "{COOKIE}=; Path=/; Max-Age=0; HttpOnly; SameSite=Strict{}",
        if secure { "; Secure" } else { "" }
    )
}

/// A signed-in admin (any route except session, login and logout).
pub struct Admin;

impl FromRequestParts<AdminState> for Admin {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, st: &AdminState) -> Result<Self, Self::Rejection> {
        match cookie_token(&parts.headers) {
            Some(t) if st.sessions.valid(&t) => Ok(Admin),
            _ => Err(ApiError::unauthorized()),
        }
    }
}

/// `GET /api/session`
pub async fn session(State(st): State<AdminState>, headers: HeaderMap) -> Json<serde_json::Value> {
    let authenticated = cookie_token(&headers).is_some_and(|t| st.sessions.valid(&t));
    Json(json!({ "enabled": true, "authenticated": authenticated }))
}

#[derive(Deserialize)]
pub struct LoginBody {
    password: String,
}

/// `POST /api/login`
pub async fn login(
    State(st): State<AdminState>,
    PeerAddr(peer): PeerAddr,
    headers: HeaderMap,
    Body(body): Body<LoginBody>,
) -> Response {
    let ip = client_ip(&headers, peer);
    if !st.login_ip.allow(ip.clone()) || !st.login_all.allow(()) {
        warn!(%ip, "admin login rate limited");
        return ApiError::rate_limited().with_retry_after(60).into_response();
    }
    let ok = body.password.len() <= MAX_PASSWORD_BYTES && ct_eq(&digest(&body.password), &st.password_digest);
    if !ok {
        warn!(%ip, "admin login failed");
        tokio::time::sleep(FAIL_DELAY).await;
        return ApiError::new(StatusCode::UNAUTHORIZED, "wrong_password", "Wrong password").into_response();
    }
    info!(%ip, "admin signed in");
    let token = st.sessions.create();
    let cookie = session_cookie(&token, secure(&st, &headers));
    let mut res = Json(json!({ "ok": true })).into_response();
    if let Ok(v) = HeaderValue::from_str(&cookie) {
        res.headers_mut().insert(header::SET_COOKIE, v);
    }
    res
}

/// `POST /api/logout`
pub async fn logout(State(st): State<AdminState>, headers: HeaderMap) -> Response {
    if let Some(t) = cookie_token(&headers) {
        st.sessions.remove(&t);
    }
    let mut res = Json(json!({ "ok": true })).into_response();
    if let Ok(v) = HeaderValue::from_str(&clear_cookie(secure(&st, &headers))) {
        res.headers_mut().insert(header::SET_COOKIE, v);
    }
    res
}

/// The authority (`host[:port]`) of an `Origin` value, lowercased.
fn origin_authority(origin: &str) -> Option<String> {
    let rest = origin
        .strip_prefix("https://")
        .or_else(|| origin.strip_prefix("http://"))?;
    let authority = rest.split('/').next()?;
    (!authority.is_empty()).then(|| authority.to_ascii_lowercase())
}

/// Why a mutating request is refused as a possible cross-site request, if it is.
pub fn csrf_refusal(method: &Method, headers: &HeaderMap) -> Option<&'static str> {
    if matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS) {
        return None;
    }
    let value = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).map(str::trim);
    if value(CSRF_HEADER) != Some("1") {
        return Some("missing admin header");
    }
    if let Some(site) = value("sec-fetch-site") {
        if !site.eq_ignore_ascii_case("same-origin") && !site.eq_ignore_ascii_case("none") {
            return Some("cross-site request");
        }
    }
    if let Some(origin) = value("origin") {
        let host = value("host").map(str::to_ascii_lowercase);
        if origin_authority(origin).is_none() || origin_authority(origin) != host {
            return Some("origin does not match");
        }
    }
    None
}

/// Refuse mutating requests that could come from another site.
pub async fn csrf_guard(req: Request, next: Next) -> Response {
    if let Some(reason) = csrf_refusal(req.method(), req.headers()) {
        warn!(reason, path = %req.uri().path(), "admin request refused (csrf)");
        return ApiError::new(StatusCode::FORBIDDEN, "csrf", "Request refused").into_response();
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_compare() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"abcd"));
        assert!(ct_eq(&digest("pw"), &digest("pw")));
        assert!(!ct_eq(&digest("pw"), &digest("pw ")));
    }

    #[test]
    fn sessions_expire_and_stay_bounded() {
        let s = AdminSessions::new(Duration::from_secs(60), 2);
        let a = s.create();
        assert_eq!(a.len(), 64);
        assert!(s.valid(&a));
        assert!(!s.valid("x"));
        assert!(!s.valid(&"0".repeat(64)));
        let _b = s.create();
        let c = s.create();
        assert_eq!(s.len(), 2);
        assert!(!s.valid(&a), "oldest dropped when full");
        s.remove(&c);
        assert!(!s.valid(&c));
        let expired = AdminSessions::new(Duration::ZERO, 2);
        let t = expired.create();
        assert!(!expired.valid(&t));
    }

    #[test]
    fn client_ip_trusts_real_ip_only_from_nginx() {
        let mut h = HeaderMap::new();
        h.insert("x-real-ip", "203.0.113.9".parse().unwrap());
        h.insert("x-forwarded-for", "198.51.100.1".parse().unwrap());
        let local: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        let remote: SocketAddr = "192.0.2.4:5000".parse().unwrap();
        assert_eq!(client_ip(&h, Some(local)), "203.0.113.9");
        assert_eq!(
            client_ip(&h, Some(remote)),
            "192.0.2.4",
            "a direct client cannot pick its address"
        );
        h.insert("x-real-ip", "not an ip".parse().unwrap());
        assert_eq!(client_ip(&h, Some(local)), "127.0.0.1");
    }

    #[test]
    fn csrf_rules() {
        let post = Method::POST;
        let mut h = HeaderMap::new();
        assert!(csrf_refusal(&Method::GET, &h).is_none(), "reads pass");
        assert_eq!(csrf_refusal(&post, &h), Some("missing admin header"));
        h.insert(CSRF_HEADER, "1".parse().unwrap());
        assert!(
            csrf_refusal(&post, &h).is_none(),
            "no Origin (same-origin fetch in some browsers)"
        );
        h.insert("host", "lynx.lan:81".parse().unwrap());
        h.insert("origin", "http://lynx.lan:81".parse().unwrap());
        assert!(csrf_refusal(&post, &h).is_none());
        h.insert("origin", "https://evil.example".parse().unwrap());
        assert_eq!(csrf_refusal(&post, &h), Some("origin does not match"));
        h.insert("origin", "null".parse().unwrap());
        assert_eq!(csrf_refusal(&post, &h), Some("origin does not match"));
        h.insert("origin", "http://lynx.lan:81".parse().unwrap());
        h.insert("sec-fetch-site", "cross-site".parse().unwrap());
        assert_eq!(csrf_refusal(&post, &h), Some("cross-site request"));
        h.insert("sec-fetch-site", "same-origin".parse().unwrap());
        assert!(csrf_refusal(&Method::DELETE, &h).is_none());
    }

    #[test]
    fn cookie_flags() {
        let c = session_cookie("abc", false);
        assert_eq!(
            c,
            format!(
                "lynx_admin=abc; Path=/; Max-Age={}; HttpOnly; SameSite=Strict",
                SESSION_TTL.as_secs()
            )
        );
        assert!(session_cookie("abc", true).ends_with("; Secure"));
        assert!(clear_cookie(false).contains("Max-Age=0"));
        let mut h = HeaderMap::new();
        h.insert(header::COOKIE, "a=1; lynx_admin=tok ; b=2".parse().unwrap());
        assert_eq!(cookie_token(&h).as_deref(), Some("tok"));
    }
}
