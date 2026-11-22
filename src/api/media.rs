//! Image proxies: the Activity iframe only loads mapped origins, so Discord CDN images go through
//! `/cdn/*` and track artwork through `/img?url=`. Both are restricted, size capped and cached.

use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Deserialize;

use super::{
    error::{ApiError, ApiResult, Params},
    ApiState,
};

const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;
const CACHE_CONTROL: &str = "public, max-age=86400, immutable";
/// Discord CDN prefixes the panel may load (avatars, guild icons and member avatars, defaults).
const CDN_PREFIXES: &[&str] = &["avatars/", "icons/", "guilds/", "embed/avatars/"];
/// Artwork hosts: exact names and `*.suffix` wildcards.
const IMG_HOSTS: &[&str] = &[
    "i.ytimg.com",
    "i1.ytimg.com",
    "i2.ytimg.com",
    "i3.ytimg.com",
    "i4.ytimg.com",
    "i9.ytimg.com",
    "*.ggpht.com",
    "lh3.googleusercontent.com",
    "lh4.googleusercontent.com",
    "lh5.googleusercontent.com",
    "lh6.googleusercontent.com",
    "yt3.googleusercontent.com",
    "i.scdn.co",
    "mosaic.scdn.co",
    "image-cdn-ak.spotifycdn.com",
    "image-cdn-fa.spotifycdn.com",
    "*.sndcdn.com",
    "*.bcbits.com",
    "*.mzstatic.com",
    "e-cdns-images.dzcdn.net",
    "cdn-images.dzcdn.net",
    "*.vimeocdn.com",
    "*.twimg.com",
    "cdn.discordapp.com",
    "media.discordapp.net",
];

/// Rewrite an absolute image URL to the backend proxies (as the panel sees them, with `/backend`).
pub fn rewrite(url: &str) -> String {
    if let Some(rest) = url.strip_prefix("https://cdn.discordapp.com/") {
        return format!("/backend/cdn/{rest}");
    }
    if url.starts_with("https://") || url.starts_with("http://") {
        return format!("/backend/img?url={}", encode(url));
    }
    url.to_owned()
}

pub fn rewrite_opt(url: Option<String>) -> Option<String> {
    url.map(|u| rewrite(&u))
}

fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3 / 2);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// `avatars/1/abc.png` style paths only; no traversal, no odd characters.
pub fn cdn_path_allowed(path: &str) -> bool {
    path.len() <= 256
        && CDN_PREFIXES.iter().any(|p| path.starts_with(p))
        && !path.contains("..")
        && !path.contains("//")
        && path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'-'))
}

fn host_allowed(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    IMG_HOSTS.iter().any(|h| match h.strip_prefix("*.") {
        Some(suffix) => host.ends_with(&format!(".{suffix}")),
        None => host == *h,
    })
}

/// Only https URLs on the artwork allowlist, default port, no credentials.
pub fn img_url_allowed(raw: &str) -> Option<reqwest::Url> {
    if raw.len() > 2048 {
        return None;
    }
    let url = reqwest::Url::parse(raw).ok()?;
    let ok = url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none()
        && url.host_str().is_some_and(host_allowed);
    ok.then_some(url)
}

pub async fn cdn(State(st): State<ApiState>, Path(path): Path<String>, query: axum::http::Uri) -> ApiResult<Response> {
    if !cdn_path_allowed(&path) {
        return Err(ApiError::not_found("not a proxied path"));
    }
    let mut url = format!("https://cdn.discordapp.com/{path}");
    if let Some(q) = query.query() {
        // Only the size parameter is meaningful for avatars and icons.
        if let Some(size) = q.split('&').find_map(|kv| kv.strip_prefix("size=")) {
            if size.len() <= 4 && size.bytes().all(|b| b.is_ascii_digit()) {
                url.push_str("?size=");
                url.push_str(size);
            }
        }
    }
    let url = reqwest::Url::parse(&url).map_err(|_| ApiError::invalid("bad path"))?;
    fetch(&st, url).await
}

#[derive(Deserialize)]
pub struct ImgQuery {
    url: String,
}

pub async fn img(State(st): State<ApiState>, Params(q): Params<ImgQuery>) -> ApiResult<Response> {
    let url = img_url_allowed(&q.url).ok_or_else(|| ApiError::invalid("host not allowed"))?;
    fetch(&st, url).await
}

/// Fetch an image with the size cap and content-type check, then stream it back with caching.
async fn fetch(st: &ApiState, url: reqwest::Url) -> ApiResult<Response> {
    let res = st
        .media
        .get(url)
        .send()
        .await
        .map_err(|_| ApiError::new(StatusCode::BAD_GATEWAY, "unavailable", "image fetch failed"))?;
    if !res.status().is_success() {
        let status = if res.status() == StatusCode::NOT_FOUND {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::BAD_GATEWAY
        };
        return Err(ApiError::new(status, "not_found", "image not available"));
    }
    let content_type = res
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !content_type.starts_with("image/") || content_type.contains("svg") {
        return Err(ApiError::new(StatusCode::BAD_GATEWAY, "invalid", "not an image"));
    }
    if res.content_length().is_some_and(|l| l as usize > MAX_IMAGE_BYTES) {
        return Err(ApiError::new(StatusCode::BAD_GATEWAY, "invalid", "image too large"));
    }
    let mut res = res;
    let mut body = Vec::new();
    while let Some(chunk) = res
        .chunk()
        .await
        .map_err(|_| ApiError::new(StatusCode::BAD_GATEWAY, "unavailable", "image fetch failed"))?
    {
        if body.len() + chunk.len() > MAX_IMAGE_BYTES {
            return Err(ApiError::new(StatusCode::BAD_GATEWAY, "invalid", "image too large"));
        }
        body.extend_from_slice(&chunk);
    }
    let mut out = Response::new(Body::from(body));
    let h = out.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&content_type) {
        h.insert(header::CONTENT_TYPE, v);
    }
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(CACHE_CONTROL));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    Ok(out.into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlists() {
        assert!(cdn_path_allowed("avatars/1/abc.png"));
        assert!(cdn_path_allowed("embed/avatars/3.png"));
        assert!(cdn_path_allowed("guilds/1/users/2/avatars/x.png"));
        assert!(!cdn_path_allowed("attachments/1/2/file.exe"));
        assert!(!cdn_path_allowed("avatars/../attachments/x"));
        assert!(!cdn_path_allowed("avatars/1/a.png?x=1"));
        assert!(img_url_allowed("https://i.ytimg.com/vi/x/hqdefault.jpg").is_some());
        assert!(img_url_allowed("https://yt3.ggpht.com/abc").is_some());
        assert!(img_url_allowed("https://i1.sndcdn.com/a.jpg").is_some());
        assert!(img_url_allowed("http://i.ytimg.com/vi/x.jpg").is_none(), "https only");
        assert!(img_url_allowed("https://evil.com/x.jpg").is_none());
        assert!(img_url_allowed("https://i.ytimg.com.evil.com/x.jpg").is_none());
        assert!(img_url_allowed("https://evilggpht.com/x.jpg").is_none());
        assert!(img_url_allowed("https://i.ytimg.com:8443/x.jpg").is_none());
        assert!(img_url_allowed("https://user@i.ytimg.com/x.jpg").is_none());
        assert!(img_url_allowed("https://127.0.0.1/x.jpg").is_none());
    }

    #[test]
    fn rewrites() {
        assert_eq!(
            rewrite("https://cdn.discordapp.com/avatars/1/a.png?size=128"),
            "/backend/cdn/avatars/1/a.png?size=128"
        );
        assert_eq!(
            rewrite("https://i.ytimg.com/vi/x/hq.jpg"),
            "/backend/img?url=https%3A%2F%2Fi.ytimg.com%2Fvi%2Fx%2Fhq.jpg"
        );
        assert_eq!(rewrite("/backend/cdn/x"), "/backend/cdn/x");
    }
}
