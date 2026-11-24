//! SSRF guard for links users hand to the bot and for the stream URLs yt-dlp returns for them.
//!
//! Three layers:
//! - [`check_link`]: a user link (anything that is not YouTube / Spotify / Apple Music) must be
//!   `http(s)`, carry no credentials, use the scheme's default port, be on an allowed site
//!   ([`LinkHosts`], `LINK_HOSTS`) and resolve only to public addresses. Only then does yt-dlp see it
//!   (and yt-dlp itself only runs the extractors in `YTDLP_EXTRACTORS`, never the generic one).
//! - [`check_static`]: the URL-only part (no DNS), applied to every stream URL yt-dlp returns and to
//!   every redirect hop of the guarded client.
//! - [`guarded`]: the reqwest client that fetches stream URLs resolves names through
//!   [`PublicResolver`], which drops non-public addresses at connect time (so a DNS answer that
//!   changes after the check cannot reach an internal address), and re-checks each redirect hop.
//!
//! IP literals do not go through a resolver in reqwest, so a URL with a literal host is only safe
//! when [`check_static`] ran on it; every URL the player fetches comes from a checked `StreamInfo`.

use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use reqwest::{
    dns::{Addrs, Name, Resolve, Resolving},
    redirect, ClientBuilder, Url,
};

/// Text users get for a link that cannot be played, whatever the reason (blocked, unsupported by
/// the allowed extractors, extractor error). Details go to the logs only: distinct messages would
/// let a user probe hosts and ports through the bot.
pub const LINK_ERROR: &str = "Could not play that link";

/// Redirect hops the guarded client follows.
const MAX_REDIRECTS: usize = 5;
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(5);

/// Sites whose links are passed to yt-dlp by default (a host matches its domain and subdomains).
/// Keep in step with `ytdlp::DEFAULT_EXTRACTORS`.
pub const DEFAULT_LINK_HOSTS: &[&str] = &[
    "youtube.com",
    "youtu.be",
    "youtube-nocookie.com",
    "soundcloud.com",
    "snd.sc",
    "bandcamp.com",
    "vimeo.com",
    "twitch.tv",
    "mixcloud.com",
    "dailymotion.com",
    "dai.ly",
    "audiomack.com",
    "audius.co",
    "archive.org",
    "jamendo.com",
    "hearthis.at",
    "newgrounds.com",
];

/// Why a URL was refused (logged, never shown to users).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Blocked {
    #[error("not an http(s) URL")]
    Scheme,
    #[error("URL carries credentials")]
    Credentials,
    #[error("non-default port")]
    Port,
    #[error("host not allowed")]
    Host,
    #[error("site not in LINK_HOSTS")]
    Site,
    #[error("address is not public")]
    Address,
    #[error("host does not resolve")]
    Resolve,
}

/// Sites allowed for user links (`LINK_HOSTS`, comma-separated domains; `*` allows any public host).
#[derive(Debug, Clone)]
pub struct LinkHosts {
    any: bool,
    domains: Vec<String>,
}

impl Default for LinkHosts {
    fn default() -> Self {
        Self::new(DEFAULT_LINK_HOSTS.iter().map(|s| s.to_string()))
    }
}

impl LinkHosts {
    pub fn new(domains: impl IntoIterator<Item = String>) -> Self {
        let mut any = false;
        let mut list = Vec::new();
        for d in domains {
            let d = d
                .trim()
                .trim_start_matches("*.")
                .trim_end_matches('.')
                .to_ascii_lowercase();
            if d == "*" {
                any = true;
            } else if !d.is_empty() {
                list.push(d);
            }
        }
        Self { any, domains: list }
    }

    /// `LINK_HOSTS` from the config; empty means the defaults.
    pub fn from_config(list: &[String]) -> Self {
        if list.is_empty() {
            Self::default()
        } else {
            Self::new(list.iter().cloned())
        }
    }

    pub fn allows(&self, host: &str) -> bool {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        self.any
            || self
                .domains
                .iter()
                .any(|d| host == *d || host.strip_suffix(d.as_str()).is_some_and(|p| p.ends_with('.')))
    }
}

/// Whether the bot may connect to `ip` on a user's behalf: only globally routable unicast.
pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => is_public_v6(v6),
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    let blocked = a == 0 // 0.0.0.0/8 ("this network", incl. unspecified)
        || a == 10 // 10/8 private
        || a == 127 // loopback
        || (a == 100 && (b & 0xc0) == 64) // 100.64/10 CGNAT
        || (a == 169 && b == 254) // link-local (cloud metadata)
        || (a == 172 && (b & 0xf0) == 16) // 172.16/12 private
        || (a == 192 && b == 0 && c == 0) // 192.0.0/24 IETF protocol assignments
        || (a == 192 && b == 0 && c == 2) // TEST-NET-1
        || (a == 192 && b == 88 && c == 99) // 6to4 relay anycast
        || (a == 192 && b == 168) // private
        || (a == 198 && (b & 0xfe) == 18) // 198.18/15 benchmarking
        || (a == 198 && b == 51 && c == 100) // TEST-NET-2
        || (a == 203 && b == 0 && c == 113) // TEST-NET-3
        || a >= 224; // multicast, 240/4 reserved, broadcast
    !blocked
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    // ::ffff:a.b.c.d (IPv4-mapped): the IPv4 rules decide.
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_public_v4(v4);
    }
    // 64:ff9b::/96 (NAT64 well-known prefix) embeds an IPv4 address; 64:ff9b:1::/48 is local use.
    if s[0] == 0x64 && s[1] == 0xff9b {
        return s[2..6] == [0, 0, 0, 0] && is_public_v4(embedded_v4(s[6], s[7]));
    }
    // 2002::/16 (6to4) embeds an IPv4 address in the next 32 bits.
    if s[0] == 0x2002 {
        return is_public_v4(embedded_v4(s[1], s[2]));
    }
    // Everything else must be global unicast (2000::/3): this rules out ::, ::1, IPv4-compatible
    // ::a.b.c.d, 100::/64 discard, fc00::/7 ULA, fe80::/10 link-local, fec0::/10, ff00::/8 multicast.
    if s[0] & 0xe000 != 0x2000 {
        return false;
    }
    let blocked = (s[0] == 0x2001 && s[1] < 0x0200) // 2001::/23 IETF (Teredo, benchmarking, ORCHID)
        || (s[0] == 0x2001 && s[1] == 0x0db8) // documentation
        || (s[0] == 0x3fff && s[1] < 0x1000); // 3fff::/20 documentation
    !blocked
}

fn embedded_v4(hi: u16, lo: u16) -> Ipv4Addr {
    let [a, b] = hi.to_be_bytes();
    let [c, d] = lo.to_be_bytes();
    Ipv4Addr::new(a, b, c, d)
}

/// The DNS-free checks: `http(s)`, no credentials, a host that is not an internal name or a
/// non-public IP literal, and (with `default_port_only`) no explicit port.
pub fn check_static(url: &Url, default_port_only: bool) -> Result<(), Blocked> {
    check_static_with(url, default_port_only, is_public_ip)
}

fn check_static_with(url: &Url, default_port_only: bool, allow: fn(IpAddr) -> bool) -> Result<(), Blocked> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(Blocked::Scheme);
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(Blocked::Credentials);
    }
    // `Url` drops the scheme's default port, so any port left is a non-default one.
    if default_port_only && url.port().is_some() {
        return Err(Blocked::Port);
    }
    match url.host() {
        None => Err(Blocked::Host),
        Some(url::Host::Ipv4(ip)) => allow(IpAddr::V4(ip)).then_some(()).ok_or(Blocked::Address),
        Some(url::Host::Ipv6(ip)) => allow(IpAddr::V6(ip)).then_some(()).ok_or(Blocked::Address),
        Some(url::Host::Domain(d)) => {
            let d = d.trim_end_matches('.').to_ascii_lowercase();
            let internal = d.is_empty()
                || d == "localhost"
                || [".localhost", ".local", ".internal", ".home.arpa"]
                    .iter()
                    .any(|s| d.ends_with(s))
                || !d.contains('.');
            if internal {
                Err(Blocked::Host)
            } else {
                Ok(())
            }
        }
    }
}

/// Full check of a user link before yt-dlp may open it: [`check_static`] with default ports only,
/// an allowed site, and every address the host resolves to public.
pub async fn check_link(raw: &str, hosts: &LinkHosts) -> Result<Url, Blocked> {
    let url = Url::parse(raw).map_err(|_| Blocked::Scheme)?;
    check_static(&url, true)?;
    let Some(url::Host::Domain(domain)) = url.host() else {
        // A public IP literal: no site to match it against.
        return if hosts.any { Ok(url) } else { Err(Blocked::Site) };
    };
    if !hosts.allows(domain) {
        return Err(Blocked::Site);
    }
    let port = url.port_or_known_default().unwrap_or(443);
    let addrs: Vec<SocketAddr> = tokio::time::timeout(RESOLVE_TIMEOUT, tokio::net::lookup_host((domain, port)))
        .await
        .map_err(|_| Blocked::Resolve)?
        .map_err(|_| Blocked::Resolve)?
        .collect();
    if addrs.is_empty() {
        return Err(Blocked::Resolve);
    }
    if addrs.iter().any(|a| !is_public_ip(a.ip())) {
        return Err(Blocked::Address);
    }
    Ok(url)
}

/// DNS resolver that only hands out addresses passing `allow` (public ones in production).
#[derive(Clone, Copy)]
pub struct PublicResolver {
    allow: fn(IpAddr) -> bool,
}

impl Default for PublicResolver {
    fn default() -> Self {
        Self { allow: is_public_ip }
    }
}

impl Resolve for PublicResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let allow = self.allow;
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let found: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            let public: Vec<SocketAddr> = found.into_iter().filter(|a| allow(a.ip())).collect();
            if public.is_empty() {
                return Err(Box::new(Blocked::Address) as Box<dyn std::error::Error + Send + Sync>);
            }
            Ok(Box::new(public.into_iter()) as Addrs)
        })
    }
}

/// Guard a client that fetches URLs users can influence (stream URLs): public addresses only at
/// connect time, and every redirect hop re-checked.
pub fn guarded(builder: ClientBuilder) -> ClientBuilder {
    guarded_with(builder, is_public_ip)
}

fn guarded_with(builder: ClientBuilder, allow: fn(IpAddr) -> bool) -> ClientBuilder {
    builder
        .dns_resolver(Arc::new(PublicResolver { allow }))
        .redirect(redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= MAX_REDIRECTS {
                return attempt.error("too many redirects");
            }
            match check_static_with(attempt.url(), false, allow) {
                Ok(()) => attempt.follow(),
                Err(b) => attempt.error(b),
            }
        }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn blocked_and_public_addresses() {
        let cases: &[(&str, bool)] = &[
            // IPv4
            ("0.0.0.0", false),
            ("0.1.2.3", false),
            ("127.0.0.1", false),
            ("127.255.255.254", false),
            ("10.0.0.1", false),
            ("172.16.0.1", false),
            ("172.31.255.255", false),
            ("192.168.1.1", false),
            ("169.254.169.254", false),
            ("100.64.0.1", false),
            ("100.127.255.255", false),
            ("192.0.0.8", false),
            ("192.0.2.1", false),
            ("198.18.0.1", false),
            ("198.51.100.1", false),
            ("203.0.113.1", false),
            ("224.0.0.1", false),
            ("239.255.255.250", false),
            ("240.0.0.1", false),
            ("255.255.255.255", false),
            ("8.8.8.8", true),
            ("1.1.1.1", true),
            ("172.15.255.255", true),
            ("172.32.0.1", true),
            ("100.63.255.255", true),
            ("100.128.0.1", true),
            ("142.250.180.14", true),
            // IPv6
            ("::", false),
            ("::1", false),
            ("::127.0.0.1", false),
            ("::ffff:127.0.0.1", false),
            ("::ffff:10.1.2.3", false),
            ("::ffff:169.254.169.254", false),
            ("::ffff:8.8.8.8", true),
            ("64:ff9b::7f00:1", false),
            ("64:ff9b::808:808", true),
            ("64:ff9b:1::1", false),
            ("2002:7f00:1::", false),
            ("2002:0808:0808::1", true),
            ("fc00::1", false),
            ("fd12:3456::1", false),
            ("fe80::1", false),
            ("fec0::1", false),
            ("ff02::1", false),
            ("100::1", false),
            ("2001:db8::1", false),
            ("2001::1", false),
            ("2001:2::1", false),
            ("3fff::1", false),
            ("2606:4700:4700::1111", true),
            ("2a00:1450:4001:80b::200e", true),
        ];
        for (ip, public) in cases {
            let parsed: IpAddr = ip.parse().unwrap();
            assert_eq!(is_public_ip(parsed), *public, "{ip}");
        }
    }

    #[test]
    fn static_url_checks() {
        let check = |u: &str, strict: bool| check_static(&Url::parse(u).unwrap(), strict);
        assert_eq!(check("https://soundcloud.com/a/b", true), Ok(()));
        assert_eq!(check("ftp://soundcloud.com/a", true), Err(Blocked::Scheme));
        assert_eq!(check("file:///etc/passwd", true), Err(Blocked::Scheme));
        assert_eq!(
            check("https://user:pw@soundcloud.com/a", true),
            Err(Blocked::Credentials)
        );
        assert_eq!(check("https://user@soundcloud.com/a", true), Err(Blocked::Credentials));
        assert_eq!(check("https://soundcloud.com:8443/a", true), Err(Blocked::Port));
        assert_eq!(check("https://soundcloud.com:443/a", true), Ok(()));
        assert_eq!(check("https://cdn.example.com:8443/a", false), Ok(()));
        assert_eq!(check("http://127.0.0.1/", false), Err(Blocked::Address));
        // Integer / short forms are normalised to IPv4 by the URL parser.
        assert_eq!(check("http://2130706433/", false), Err(Blocked::Address));
        assert_eq!(check("http://0x7f.1/", false), Err(Blocked::Address));
        assert_eq!(check("http://[::1]/", false), Err(Blocked::Address));
        assert_eq!(check("http://[::ffff:169.254.169.254]/", false), Err(Blocked::Address));
        assert_eq!(
            check("http://169.254.169.254/latest/meta-data", false),
            Err(Blocked::Address)
        );
        assert_eq!(check("http://localhost/", false), Err(Blocked::Host));
        assert_eq!(check("http://LOCALHOST./", false), Err(Blocked::Host));
        assert_eq!(check("http://a.localhost/", false), Err(Blocked::Host));
        assert_eq!(check("http://metadata.google.internal/", false), Err(Blocked::Host));
        assert_eq!(check("http://nas/", false), Err(Blocked::Host));
        assert_eq!(check("https://8.8.8.8/x", true), Ok(()));
        // The fragment trick that fools a `[^/]+\.bandcamp\.com` extractor regex: the real host is 127.0.0.1.
        assert_eq!(
            check("http://127.0.0.1#.bandcamp.com/track/x", true),
            Err(Blocked::Address)
        );
    }

    #[test]
    fn link_hosts_match_domains_and_subdomains() {
        let h = LinkHosts::default();
        assert!(h.allows("soundcloud.com"));
        assert!(h.allows("m.soundcloud.com"));
        assert!(h.allows("artist.bandcamp.com"));
        assert!(h.allows("SoundCloud.com."));
        assert!(!h.allows("evilsoundcloud.com"));
        assert!(!h.allows("soundcloud.com.evil.net"));
        assert!(!h.allows("example.com"));
        let any = LinkHosts::new(["*".to_owned()]);
        assert!(any.allows("example.com"));
        assert!(LinkHosts::from_config(&[]).allows("vimeo.com"));
        assert!(!LinkHosts::from_config(&["vimeo.com".into()]).allows("soundcloud.com"));
    }

    #[tokio::test]
    async fn links_are_checked_before_yt_dlp() {
        let any = LinkHosts::new(["*".to_owned()]);
        assert_eq!(
            check_link("http://127.0.0.1/x", &any).await.unwrap_err(),
            Blocked::Address
        );
        assert_eq!(check_link("http://localhost/x", &any).await.unwrap_err(), Blocked::Host);
        assert_eq!(
            check_link("https://example.com:22/", &any).await.unwrap_err(),
            Blocked::Port
        );
        assert_eq!(
            check_link("https://example.com/", &LinkHosts::default())
                .await
                .unwrap_err(),
            Blocked::Site
        );
        assert_eq!(check_link("not a url", &any).await.unwrap_err(), Blocked::Scheme);
        // A public IP literal passes only with `*`.
        assert!(check_link("https://8.8.8.8/x", &any).await.is_ok());
        assert_eq!(
            check_link("https://8.8.8.8/x", &LinkHosts::default())
                .await
                .unwrap_err(),
            Blocked::Site
        );
    }

    #[tokio::test]
    async fn resolver_drops_private_addresses() {
        let r = PublicResolver::default();
        let name: Name = "localhost".parse().unwrap();
        assert!(r.resolve(name).await.is_err());
    }

    /// One-shot HTTP server on 127.0.0.1 answering every request with `response`.
    async fn serve(response: String) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let response = response.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let _ = sock.read(&mut buf).await;
                    let _ = sock.write_all(response.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        port
    }

    fn redirect_to(location: &str) -> String {
        format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
    }

    /// Tests reach their own server on 127.0.0.1; everything else follows the production rules.
    fn loopback_test_server(ip: IpAddr) -> bool {
        ip == IpAddr::V4(Ipv4Addr::LOCALHOST) || is_public_ip(ip)
    }

    fn test_client() -> reqwest::Client {
        guarded_with(reqwest::Client::builder(), loopback_test_server)
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn redirect_to_private_host_is_refused() {
        for target in [
            "http://169.254.169.254/latest/meta-data/",
            "http://10.0.0.1/",
            "http://[::1]/",
            "http://localhost/",
        ] {
            let port = serve(redirect_to(target)).await;
            let err = test_client()
                .get(format!("http://127.0.0.1:{port}/"))
                .send()
                .await
                .unwrap_err();
            assert!(err.is_redirect(), "{target}: {err:?}");
        }
        // Non-http(s) targets are never followed (reqwest hands back the redirect itself).
        let port = serve(redirect_to("file:///etc/passwd")).await;
        let res = test_client()
            .get(format!("http://127.0.0.1:{port}/"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 302);
    }

    #[tokio::test]
    async fn redirect_to_allowed_host_is_followed() {
        let ok = serve("HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".into()).await;
        let port = serve(redirect_to(&format!("http://127.0.0.1:{ok}/final"))).await;
        let res = test_client()
            .get(format!("http://127.0.0.1:{port}/"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        assert_eq!(res.text().await.unwrap(), "ok");
    }

    #[tokio::test]
    async fn too_many_redirects_stop() {
        let port = serve(redirect_to("/again")).await;
        let err = test_client()
            .get(format!("http://127.0.0.1:{port}/"))
            .send()
            .await
            .unwrap_err();
        assert!(err.is_redirect());
    }
}
