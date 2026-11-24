//! Reads the git revision of a published image from GitHub Container Registry (registry API v2).
//!
//! 1. A pull token from `https://ghcr.io/token` (anonymous for public packages; with `GHCR_TOKEN`
//!    as Basic credentials for private ones).
//! 2. The manifest of the tag. A multi-arch index (what buildx publishes, with attestations) is
//!    resolved to its `linux/amd64` image manifest.
//! 3. The image config blob, whose `org.opencontainers.image.revision` label is the commit
//!    (docker/metadata-action sets it to the full git SHA). Fallbacks: a `sha-<short>` tag, then the
//!    `org.opencontainers.image.version` label when it is a hash.
//!
//! Only `ghcr.io` is contacted (the image reference is validated and the host is fixed), every
//! request has a timeout and every body a size cap. The token is never logged.

use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use reqwest::{header, StatusCode};
use serde_json::Value;

pub const REGISTRY: &str = "ghcr.io";
const BASE: &str = "https://ghcr.io";
const MAX_MANIFEST: usize = 1024 * 1024;
const MAX_CONFIG: usize = 4 * 1024 * 1024;
const MAX_TOKEN: usize = 64 * 1024;
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
const DOCKER_LIST: &str = "application/vnd.docker.distribution.manifest.list.v2+json";
const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
const DOCKER_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";

const LABEL_REVISION: &str = "org.opencontainers.image.revision";
const LABEL_VERSION: &str = "org.opencontainers.image.version";

/// A validated `ghcr.io/<owner>/<name>[:<tag>]` reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef {
    /// `owner/name` (lowercase, as GHCR requires).
    pub repo: String,
    pub tag: String,
}

impl std::fmt::Display for ImageRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{REGISTRY}/{}:{}", self.repo, self.tag)
    }
}

fn path_component_ok(c: &str) -> bool {
    // Distribution spec: [a-z0-9]+ separated by single '.', '_', '__' or '-' runs.
    let b = c.as_bytes();
    !b.is_empty()
        && b.first().is_some_and(u8::is_ascii_alphanumeric)
        && b.last().is_some_and(u8::is_ascii_alphanumeric)
        && b.iter()
            .all(|&x| x.is_ascii_lowercase() || x.is_ascii_digit() || matches!(x, b'.' | b'_' | b'-'))
        && !c.contains("..")
}

fn tag_ok(t: &str) -> bool {
    let b = t.as_bytes();
    (1..=128).contains(&b.len())
        && (b[0].is_ascii_alphanumeric() || b[0] == b'_')
        && b.iter()
            .all(|&x| x.is_ascii_alphanumeric() || matches!(x, b'_' | b'.' | b'-'))
}

impl ImageRef {
    /// Parse a reference; the error says what is wrong (shown in the admin panel).
    pub fn parse(raw: &str) -> Result<Self, &'static str> {
        let raw = raw.trim();
        if raw.contains('@') {
            return Err("use a tag, not a digest");
        }
        let rest = raw
            .strip_prefix("ghcr.io/")
            .ok_or("the image must be on ghcr.io, like ghcr.io/owner/lynx:lynx")?;
        let (repo, tag) = match rest.rsplit_once(':') {
            Some((repo, tag)) => (repo, tag),
            None => (rest, "latest"),
        };
        if repo.len() > 255 || repo.split('/').count() < 2 || !repo.split('/').all(path_component_ok) {
            return Err("the image name must be lowercase, like ghcr.io/owner/lynx");
        }
        if !tag_ok(tag) {
            return Err("the tag may only have letters, digits, '.', '_' and '-'");
        }
        Ok(Self {
            repo: repo.to_owned(),
            tag: tag.to_owned(),
        })
    }
}

/// Lowercase hex commit from a label or tag value (`sha-` prefix allowed); None when not a hash.
pub fn normalize_commit(raw: &str) -> Option<String> {
    let c = raw.trim();
    let c = c.strip_prefix("sha-").unwrap_or(c);
    ((7..=64).contains(&c.len()) && c.bytes().all(|b| b.is_ascii_hexdigit())).then(|| c.to_ascii_lowercase())
}

/// Whether two commits are the same, comparing a short hash with a full one by prefix.
pub fn same_commit(a: &str, b: &str) -> bool {
    match (normalize_commit(a), normalize_commit(b)) {
        (Some(a), Some(b)) => a.starts_with(&b) || b.starts_with(&a),
        _ => false,
    }
}

/// The first 7 characters of a commit (what the panel shows).
pub fn short(commit: &str) -> String {
    commit.chars().take(7).collect()
}

/// A parsed manifest response.
#[derive(Debug, PartialEq, Eq)]
pub enum Manifest {
    /// Multi-arch index: `(digest, os, architecture)` per entry.
    Index(Vec<(String, String, String)>),
    /// Single image: the config blob digest.
    Image { config: String },
}

fn digest_ok(d: &str) -> bool {
    d.strip_prefix("sha256:")
        .is_some_and(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Parse a manifest body. `content_type` is the response header (the body's `mediaType` wins).
pub fn parse_manifest(body: &[u8], content_type: Option<&str>) -> Result<Manifest> {
    let v: Value = serde_json::from_slice(body).context("the manifest is not JSON")?;
    let media = v["mediaType"]
        .as_str()
        .or_else(|| content_type.map(|c| c.split(';').next().unwrap_or("").trim()))
        .unwrap_or("");
    let is_index = media == OCI_INDEX || media == DOCKER_LIST || (media.is_empty() && v["manifests"].is_array());
    if is_index {
        let entries = v["manifests"]
            .as_array()
            .ok_or_else(|| anyhow!("the index has no manifests"))?
            .iter()
            .filter_map(|m| {
                let digest = m["digest"].as_str().filter(|d| digest_ok(d))?;
                let os = m["platform"]["os"].as_str().unwrap_or("").to_owned();
                let arch = m["platform"]["architecture"].as_str().unwrap_or("").to_owned();
                Some((digest.to_owned(), os, arch))
            })
            .collect();
        return Ok(Manifest::Index(entries));
    }
    if media == OCI_MANIFEST || media == DOCKER_MANIFEST || (media.is_empty() && v["config"].is_object()) {
        let config = v["config"]["digest"]
            .as_str()
            .filter(|d| digest_ok(d))
            .ok_or_else(|| anyhow!("the manifest has no config digest"))?;
        return Ok(Manifest::Image {
            config: config.to_owned(),
        });
    }
    bail!("unsupported manifest type {media:?}")
}

/// The `linux/amd64` entry of an index (the published image is amd64 only).
pub fn pick_amd64(entries: &[(String, String, String)]) -> Option<&str> {
    entries
        .iter()
        .find(|(_, os, arch)| os == "linux" && arch == "amd64")
        .map(|(d, _, _)| d.as_str())
}

/// The commit an image config says it was built from (see the module docs for the fallbacks).
pub fn revision_from_config(config: &[u8], tag: &str) -> Result<String> {
    let v: Value = serde_json::from_slice(config).context("the image config is not JSON")?;
    let labels = &v["config"]["Labels"];
    if let Some(c) = labels[LABEL_REVISION].as_str().and_then(normalize_commit) {
        return Ok(c);
    }
    if tag.starts_with("sha-") {
        if let Some(c) = normalize_commit(tag) {
            return Ok(c);
        }
    }
    if let Some(c) = labels[LABEL_VERSION].as_str().and_then(normalize_commit) {
        return Ok(c);
    }
    bail!("the image has no {LABEL_REVISION} label")
}

/// HTTP client for the registry: timeouts, a few HTTPS-only redirects (blobs redirect to GitHub's
/// storage, which gets no Authorization header).
pub fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::custom(|a| {
            if a.previous().len() >= 5 || a.url().scheme() != "https" {
                a.stop()
            } else {
                a.follow()
            }
        }))
        .user_agent(concat!("lynx/", env!("CARGO_PKG_VERSION"), " (update check)"))
        .build()?)
}

async fn read_capped(mut res: reqwest::Response, max: usize) -> Result<Vec<u8>> {
    if res.content_length().is_some_and(|l| l > max as u64) {
        bail!("response too large");
    }
    let mut out = Vec::new();
    while let Some(chunk) = res.chunk().await? {
        out.extend_from_slice(&chunk);
        if out.len() > max {
            bail!("response too large");
        }
    }
    Ok(out)
}

fn status_error(what: &str, status: StatusCode) -> anyhow::Error {
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            anyhow!("{what}: HTTP {} (private package? set GHCR_TOKEN)", status.as_u16())
        }
        StatusCode::NOT_FOUND => anyhow!("{what}: HTTP 404 (check the image name and tag)"),
        s => anyhow!("{what}: HTTP {}", s.as_u16()),
    }
}

async fn pull_token(http: &reqwest::Client, image: &ImageRef, secret: Option<&str>) -> Result<String> {
    let url = format!("{BASE}/token?scope=repository:{}:pull&service={REGISTRY}", image.repo);
    let mut req = http.get(url);
    if let Some(t) = secret {
        req = req.basic_auth("lynx", Some(t));
    }
    let res = req
        .send()
        .await
        .map_err(|e| anyhow!("token request failed: {}", e.without_url()))?;
    if !res.status().is_success() {
        return Err(status_error("token request", res.status()));
    }
    let v: Value = serde_json::from_slice(&read_capped(res, MAX_TOKEN).await?).context("bad token answer")?;
    v["token"]
        .as_str()
        .or_else(|| v["access_token"].as_str())
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("the token answer has no token"))
}

async fn get(
    http: &reqwest::Client,
    token: &str,
    path: &str,
    accept: &str,
    max: usize,
) -> Result<(Vec<u8>, Option<String>)> {
    let res = http
        .get(format!("{BASE}/v2/{path}"))
        .bearer_auth(token)
        .header(header::ACCEPT, accept)
        .send()
        .await
        .map_err(|e| anyhow!("registry request failed: {}", e.without_url()))?;
    if !res.status().is_success() {
        return Err(status_error("registry", res.status()));
    }
    let ct = res
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    Ok((read_capped(res, max).await?, ct))
}

/// The full or short commit the published image was built from.
pub async fn published_revision(http: &reqwest::Client, image: &ImageRef, secret: Option<&str>) -> Result<String> {
    let token = pull_token(http, image, secret).await?;
    let accept = [OCI_INDEX, DOCKER_LIST, OCI_MANIFEST, DOCKER_MANIFEST].join(", ");
    let manifests = format!("{}/manifests", image.repo);
    let (body, ct) = get(
        http,
        &token,
        &format!("{manifests}/{}", image.tag),
        &accept,
        MAX_MANIFEST,
    )
    .await?;
    let config = match parse_manifest(&body, ct.as_deref())? {
        Manifest::Image { config } => config,
        Manifest::Index(entries) => {
            let digest = pick_amd64(&entries).ok_or_else(|| anyhow!("the image has no linux/amd64 build"))?;
            let (body, ct) = get(http, &token, &format!("{manifests}/{digest}"), &accept, MAX_MANIFEST).await?;
            match parse_manifest(&body, ct.as_deref())? {
                Manifest::Image { config } => config,
                Manifest::Index(_) => bail!("nested image index"),
            }
        }
    };
    let (blob, _) = get(
        http,
        &token,
        &format!("{}/blobs/{config}", image.repo),
        "application/vnd.oci.image.config.v1+json, application/vnd.docker.container.image.v1+json, application/json",
        MAX_CONFIG,
    )
    .await?;
    revision_from_config(&blob, &image.tag)
}

#[cfg(test)]
mod tests {
    use super::*;

    const AMD: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
    const ATT: &str = "sha256:2222222222222222222222222222222222222222222222222222222222222222";
    const ARM: &str = "sha256:3333333333333333333333333333333333333333333333333333333333333333";
    const CFG: &str = "sha256:4444444444444444444444444444444444444444444444444444444444444444";

    /// What buildx pushes: an OCI index with the image and its provenance attestation.
    fn index_fixture() -> String {
        format!(
            r#"{{
  "schemaVersion": 2,
  "mediaType": "{OCI_INDEX}",
  "manifests": [
    {{ "mediaType": "{OCI_MANIFEST}", "digest": "{ARM}", "size": 1, "platform": {{ "architecture": "arm64", "os": "linux" }} }},
    {{ "mediaType": "{OCI_MANIFEST}", "digest": "{AMD}", "size": 1, "platform": {{ "architecture": "amd64", "os": "linux" }} }},
    {{ "mediaType": "{OCI_MANIFEST}", "digest": "{ATT}", "size": 1,
       "annotations": {{ "vnd.docker.reference.type": "attestation-manifest" }},
       "platform": {{ "architecture": "unknown", "os": "unknown" }} }}
  ]
}}"#
        )
    }

    fn manifest_fixture() -> String {
        format!(
            r#"{{
  "schemaVersion": 2,
  "mediaType": "{OCI_MANIFEST}",
  "config": {{ "mediaType": "application/vnd.oci.image.config.v1+json", "digest": "{CFG}", "size": 7023 }},
  "layers": []
}}"#
        )
    }

    const CONFIG_FIXTURE: &str = r#"{
  "architecture": "amd64",
  "os": "linux",
  "config": {
    "Env": ["PATH=/usr/local/bin"],
    "Labels": {
      "org.opencontainers.image.created": "2026-11-23T10:00:00.000Z",
      "org.opencontainers.image.revision": "FBD1347A0B1C2D3E4F5A6B7C8D9E0F1A2B3C4D5E",
      "org.opencontainers.image.source": "https://github.com/pixlgalaxy/lynx",
      "org.opencontainers.image.version": "lynx"
    }
  },
  "rootfs": { "type": "layers", "diff_ids": [] }
}"#;

    #[test]
    fn image_references() {
        let r = ImageRef::parse(" ghcr.io/pixlgalaxy/lynx:lynx ").unwrap();
        assert_eq!((r.repo.as_str(), r.tag.as_str()), ("pixlgalaxy/lynx", "lynx"));
        assert_eq!(r.to_string(), "ghcr.io/pixlgalaxy/lynx:lynx");
        assert_eq!(ImageRef::parse("ghcr.io/a/b").unwrap().tag, "latest");
        assert_eq!(
            ImageRef::parse("ghcr.io/org/team/app:sha-fbd1347").unwrap().repo,
            "org/team/app"
        );
        assert!(ImageRef::parse("ghcr.io/my-org/my_app.x:v1.2.3").is_ok());
        for bad in [
            "",
            "pixlgalaxy/lynx:lynx",
            "docker.io/pixlgalaxy/lynx:lynx",
            "ghcr.io.evil.com/pixlgalaxy/lynx:lynx",
            "ghcr.io/PixlGalaxy/lynx:lynx",
            "ghcr.io/lynx:lynx",
            "ghcr.io/a/b:",
            "ghcr.io/a/b:-x",
            "ghcr.io/a/b:x/y",
            "ghcr.io/a/../b:x",
            "ghcr.io/a//b:x",
            "ghcr.io/a/b c:x",
            "ghcr.io/a/b:x?y",
            "ghcr.io/a/b@sha256:abc",
            "https://ghcr.io/a/b:x",
        ] {
            assert!(ImageRef::parse(bad).is_err(), "{bad}");
        }
        assert!(ImageRef::parse(&format!("ghcr.io/a/b:{}", "x".repeat(129))).is_err());
    }

    #[test]
    fn commits_compare_by_prefix() {
        assert_eq!(normalize_commit("sha-FBD1347").as_deref(), Some("fbd1347"));
        assert_eq!(normalize_commit("lynx"), None);
        assert_eq!(normalize_commit("abc12"), None, "too short");
        assert!(same_commit("fbd1347", "fbd1347a0b1c2d3e4f5a6b7c8d9e0f1a2b3c4d5e"));
        assert!(same_commit("FBD1347A0B", "fbd1347"));
        assert!(same_commit("sha-fbd1347", "fbd1347"));
        assert!(!same_commit("fbd1348", "fbd1347a0b1c2d3e4f5a6b7c8d9e0f1a2b3c4d5e"));
        assert!(!same_commit("", "fbd1347"));
        assert!(!same_commit("unknown", "unknown"));
        assert_eq!(short("fbd1347a0b1c"), "fbd1347");
    }

    #[test]
    fn index_resolves_to_the_amd64_manifest_then_the_config() {
        let Manifest::Index(entries) = parse_manifest(index_fixture().as_bytes(), None).unwrap() else {
            panic!("an index");
        };
        assert_eq!(entries.len(), 3);
        assert_eq!(pick_amd64(&entries), Some(AMD), "not the attestation, not arm64");
        assert_eq!(
            parse_manifest(manifest_fixture().as_bytes(), Some(OCI_MANIFEST)).unwrap(),
            Manifest::Image { config: CFG.into() }
        );
        assert_eq!(
            revision_from_config(CONFIG_FIXTURE.as_bytes(), "lynx").unwrap(),
            "fbd1347a0b1c2d3e4f5a6b7c8d9e0f1a2b3c4d5e"
        );
    }

    #[test]
    fn docker_media_types_and_header_fallback() {
        let list = format!(
            r#"{{"schemaVersion":2,"manifests":[{{"digest":"{AMD}","platform":{{"architecture":"amd64","os":"linux"}}}}]}}"#
        );
        // No mediaType in the body: the Content-Type header says it is a list.
        let m = parse_manifest(list.as_bytes(), Some(&format!("{DOCKER_LIST}; charset=utf-8"))).unwrap();
        assert_eq!(m, Manifest::Index(vec![(AMD.into(), "linux".into(), "amd64".into())]));
        let single = format!(r#"{{"schemaVersion":2,"mediaType":"{DOCKER_MANIFEST}","config":{{"digest":"{CFG}"}}}}"#);
        assert_eq!(
            parse_manifest(single.as_bytes(), None).unwrap(),
            Manifest::Image { config: CFG.into() }
        );
        // Without any type, the shape decides.
        let bare = format!(r#"{{"config":{{"digest":"{CFG}"}}}}"#);
        assert!(matches!(
            parse_manifest(bare.as_bytes(), None).unwrap(),
            Manifest::Image { .. }
        ));
    }

    #[test]
    fn bad_manifests_are_errors() {
        assert!(parse_manifest(b"not json", None).is_err());
        assert!(parse_manifest(br#"{"mediaType":"application/vnd.something"}"#, None).is_err());
        // A digest that is not sha256:<64 hex> never reaches a URL.
        let evil = format!(r#"{{"mediaType":"{OCI_MANIFEST}","config":{{"digest":"sha256:../../x"}}}}"#);
        assert!(parse_manifest(evil.as_bytes(), None).is_err());
        let evil = format!(
            r#"{{"mediaType":"{OCI_INDEX}","manifests":[{{"digest":"../x","platform":{{"architecture":"amd64","os":"linux"}}}}]}}"#
        );
        let Manifest::Index(entries) = parse_manifest(evil.as_bytes(), None).unwrap() else {
            panic!()
        };
        assert!(pick_amd64(&entries).is_none());
        let arm_only = format!(
            r#"{{"mediaType":"{OCI_INDEX}","manifests":[{{"digest":"{ARM}","platform":{{"architecture":"arm64","os":"linux"}}}}]}}"#
        );
        let Manifest::Index(entries) = parse_manifest(arm_only.as_bytes(), None).unwrap() else {
            panic!()
        };
        assert!(pick_amd64(&entries).is_none());
    }

    /// Live check against the real registry (network): `LYNX_LIVE_GHCR_IMAGE=ghcr.io/owner/name:tag
    /// cargo test live_registry -- --ignored`.
    #[tokio::test]
    #[ignore = "network"]
    async fn live_registry() {
        let Ok(raw) = std::env::var("LYNX_LIVE_GHCR_IMAGE") else {
            return;
        };
        let image = ImageRef::parse(&raw).unwrap();
        let rev = published_revision(&client().unwrap(), &image, None).await.unwrap();
        assert!(normalize_commit(&rev).is_some(), "{rev}");
        println!("{image} -> {rev}");
    }

    #[test]
    fn revision_fallbacks() {
        let no_rev = r#"{"config":{"Labels":{"org.opencontainers.image.version":"lynx"}}}"#;
        assert_eq!(
            revision_from_config(no_rev.as_bytes(), "sha-ABC1234").unwrap(),
            "abc1234"
        );
        assert!(revision_from_config(no_rev.as_bytes(), "lynx").is_err());
        let version = r#"{"config":{"Labels":{"org.opencontainers.image.version":"sha-def5678"}}}"#;
        assert_eq!(revision_from_config(version.as_bytes(), "lynx").unwrap(), "def5678");
        let none = r#"{"config":{}}"#;
        assert!(revision_from_config(none.as_bytes(), "lynx").is_err());
        let junk = r#"{"config":{"Labels":{"org.opencontainers.image.revision":"main"}}}"#;
        assert!(revision_from_config(junk.as_bytes(), "lynx").is_err());
        assert!(revision_from_config(b"[]", "lynx").is_err());
    }
}
