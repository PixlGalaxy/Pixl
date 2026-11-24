//! Admin sign-in: credentials check, sessions, the session cookie and the CSRF guard.

use std::{
    net::{IpAddr, SocketAddr},
    time::{Duration, Instant},
};

use argon2::{
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use axum::{
    extract::{ConnectInfo, FromRequestParts, Request, State},
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

use super::{
    error::{AdminError, Body},
    AdminState, MIN_PASSWORD_CHARS, SESSION_TTL,
};

pub const COOKIE: &str = "lynx_admin";
/// Header every mutating request must carry (a cross-site form or image cannot set it, and a
/// cross-origin script would need a CORS preflight that this server never grants).
pub const CSRF_HEADER: &str = "x-lynx-admin";
/// Wait before answering wrong credentials.
const FAIL_DELAY: Duration = Duration::from_millis(300);
const MAX_FIELD_BYTES: usize = 1024;

pub fn digest(s: &str) -> [u8; 32] {
    Sha256::digest(s.as_bytes()).into()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// SHA-256 of a token, hex: the only form a session token is kept in.
fn hash_token(token: &str) -> String {
    hex(&digest(token))
}

/// Constant-time comparison: the time taken never depends on where the inputs differ.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let diff = a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y));
    std::hint::black_box(diff) == 0
}

/// How the password is checked.
#[derive(Clone)]
enum Secret {
    /// SHA-256 of `ADMIN_PASSWORD` (compared in constant time).
    Plain([u8; 32]),
    /// `ADMIN_PASSWORD_HASH`: an Argon2 PHC string (`$argon2id$v=19$m=...`).
    Argon2(String),
}

/// `ADMIN_USERNAME` plus `ADMIN_PASSWORD` or `ADMIN_PASSWORD_HASH`.
#[derive(Clone)]
pub struct Credentials {
    username: [u8; 32],
    secret: Secret,
}

impl Credentials {
    /// None (admin off, with a warning when something was set) unless the username and a usable
    /// password or hash are given. The hash wins when both are set.
    pub fn from_parts(username: Option<String>, password: Option<String>, hash: Option<String>) -> Option<Self> {
        let any = username.is_some() || password.is_some() || hash.is_some();
        let Some(user) = username else {
            if any {
                warn!("ADMIN_USERNAME is not set; the admin panel stays off");
            }
            return None;
        };
        let secret = match (hash, password) {
            (Some(h), p) => {
                if p.is_some() {
                    warn!("both ADMIN_PASSWORD and ADMIN_PASSWORD_HASH are set; using the hash");
                }
                let ok = PasswordHash::new(&h).is_ok_and(|ph| ph.algorithm.as_str().starts_with("argon2"));
                if !ok {
                    warn!("ADMIN_PASSWORD_HASH is not an Argon2 hash (see `lynx hash-admin-password`); the admin panel stays off");
                    return None;
                }
                Secret::Argon2(h)
            }
            (None, Some(p)) if p.chars().count() >= MIN_PASSWORD_CHARS => Secret::Plain(digest(&p)),
            (None, Some(_)) => {
                warn!(
                    min = MIN_PASSWORD_CHARS,
                    "ADMIN_PASSWORD is too short; the admin panel stays off"
                );
                return None;
            }
            (None, None) => {
                warn!("ADMIN_PASSWORD or ADMIN_PASSWORD_HASH is not set; the admin panel stays off");
                return None;
            }
        };
        Some(Self {
            username: digest(&user),
            secret,
        })
    }

    /// Both fields are always checked, so the time taken does not reveal which one was wrong.
    pub async fn verify(&self, username: &str, password: &str) -> bool {
        if username.len() > MAX_FIELD_BYTES || password.len() > MAX_FIELD_BYTES {
            return false;
        }
        let user_ok = ct_eq(&digest(username), &self.username);
        let pass_ok = match &self.secret {
            Secret::Plain(d) => ct_eq(&digest(password), d),
            Secret::Argon2(phc) => {
                let (phc, password) = (phc.clone(), password.to_owned());
                // Argon2 is deliberately slow: off the async workers.
                tokio::task::spawn_blocking(move || {
                    PasswordHash::new(&phc)
                        .is_ok_and(|h| Argon2::default().verify_password(password.as_bytes(), &h).is_ok())
                })
                .await
                .unwrap_or(false)
            }
        };
        user_ok & pass_ok
    }
}

/// Argon2id hash (PHC string) of `password` with a random salt.
pub fn hash_password(password: &str) -> anyhow::Result<String> {
    let mut salt = [0u8; 16];
    getrandom::fill(&mut salt).map_err(|e| anyhow::anyhow!("random source: {e}"))?;
    let salt = SaltString::encode_b64(&salt).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .to_string())
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
        let token = hex(&buf);
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

/// The socket address when served (absent in router tests).
pub struct Peer(pub Option<SocketAddr>);

impl<S: Send + Sync> FromRequestParts<S> for Peer {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(
            parts
                .extensions
                .get::<ConnectInfo<super::limit::PeerAddr>>()
                .map(|c| c.0 .0),
        ))
    }
}

pub fn cookie_token(headers: &HeaderMap) -> Option<String> {
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

/// A signed-in admin (every route except session, login and logout). Carries the token, so a
/// long-lived stream can notice when its session ends.
pub struct Admin(pub String);

impl FromRequestParts<AdminState> for Admin {
    type Rejection = AdminError;

    async fn from_request_parts(parts: &mut Parts, st: &AdminState) -> Result<Self, Self::Rejection> {
        match cookie_token(&parts.headers) {
            Some(t) if st.sessions.valid(&t) => Ok(Admin(t)),
            _ => Err(AdminError::unauthorized()),
        }
    }
}

impl AdminState {
    pub fn session_valid(&self, token: &str) -> bool {
        self.sessions.valid(token)
    }
}

/// `GET /api/session`
pub async fn session(State(st): State<AdminState>, headers: HeaderMap) -> Json<serde_json::Value> {
    let authenticated = cookie_token(&headers).is_some_and(|t| st.sessions.valid(&t));
    Json(json!({ "enabled": true, "authenticated": authenticated }))
}

#[derive(Deserialize)]
pub struct LoginBody {
    username: String,
    password: String,
}

/// `POST /api/login`
pub async fn login(
    State(st): State<AdminState>,
    Peer(peer): Peer,
    headers: HeaderMap,
    Body(body): Body<LoginBody>,
) -> Response {
    let ip = client_ip(&headers, peer);
    if !st.login_ip.allow(ip.clone()) || !st.login_all.allow(()) {
        warn!(%ip, "admin sign-in rate limited");
        return AdminError::rate_limited().into_response();
    }
    if !st.credentials.verify(&body.username, &body.password).await {
        warn!(%ip, "admin sign-in failed");
        tokio::time::sleep(FAIL_DELAY).await;
        return AdminError::new(
            StatusCode::UNAUTHORIZED,
            "wrong_credentials",
            "Wrong username or password",
        )
        .into_response();
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
        return AdminError::new(StatusCode::FORBIDDEN, "csrf", "Request refused").into_response();
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
    fn credentials_need_a_username_and_a_good_secret() {
        let s = |v: &str| Some(v.to_owned());
        assert!(Credentials::from_parts(None, s("a long enough password"), None).is_none());
        assert!(Credentials::from_parts(s("admin"), None, None).is_none());
        assert!(Credentials::from_parts(s("admin"), s("short"), None).is_none());
        assert!(Credentials::from_parts(s("admin"), None, s("not a hash")).is_none());
        assert!(Credentials::from_parts(s("admin"), None, s("$pbkdf2-sha256$i=1$c2FsdA$aGFzaA")).is_none());
        assert!(Credentials::from_parts(s("admin"), s("a long enough password"), None).is_some());
        assert!(Credentials::from_parts(None, None, None).is_none());
    }

    #[tokio::test]
    async fn plain_and_hashed_passwords_verify_both_fields() {
        let s = |v: &str| Some(v.to_owned());
        let plain = Credentials::from_parts(s("admin"), s("a long enough password"), None).unwrap();
        assert!(plain.verify("admin", "a long enough password").await);
        assert!(!plain.verify("Admin", "a long enough password").await);
        assert!(!plain.verify("admin", "a long enough passwor").await);
        assert!(!plain.verify("", "").await);

        // Cheap parameters so the test stays fast; real hashes use Argon2id defaults.
        let params = argon2::Params::new(8, 1, 1, None).unwrap();
        let a2 = Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
        let salt = SaltString::encode_b64(b"0123456789abcdef").unwrap();
        let phc = a2.hash_password(b"hashed secret 123", &salt).unwrap().to_string();
        let hashed = Credentials::from_parts(s("ops"), s("ignored plain one"), Some(phc)).unwrap();
        assert!(hashed.verify("ops", "hashed secret 123").await);
        assert!(!hashed.verify("ops", "ignored plain one").await, "the hash wins");
        assert!(!hashed.verify("admin", "hashed secret 123").await);

        let made = hash_password("another long password").unwrap();
        assert!(made.starts_with("$argon2id$"));
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
