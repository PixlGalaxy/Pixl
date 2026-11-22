//! `/about`: build info, uptime, gateway latency, counts and the cached yt-dlp version.

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::{Duration, Instant},
};

use super::{
    ui::{cards, Message},
    Lynx,
};
use crate::{player::state::Status, sources::Sources};

/// Public source code of Lynx (same link as the panel's support card).
pub const SOURCE_URL: &str = "https://github.com/PixlGalaxy/Pixl";
pub const DEVELOPER_URL: &str = "https://github.com/PixlGalaxy";
pub const HOST_URL: &str = "https://itzgalaxy.com";

/// A cached yt-dlp version older than this is checked again (in the background) on `/about`.
const YTDLP_VERSION_TTL: Duration = Duration::from_secs(15 * 60);

static STARTED: OnceLock<Instant> = OnceLock::new();

/// Remember the process start; call first thing in `main`.
pub fn mark_start() {
    let _ = STARTED.set(Instant::now());
}

pub fn uptime() -> Duration {
    STARTED.get_or_init(Instant::now).elapsed()
}

/// Crate version.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Short git commit of the build (see `build.rs`), when known.
pub fn commit() -> Option<&'static str> {
    option_env!("LYNX_BUILD_COMMIT").filter(|c| !c.is_empty())
}

/// Resident memory of this process. Linux only (read from `/proc`, no extra dependency).
pub fn memory_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        parse_vm_rss(&status)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_vm_rss(status: &str) -> Option<u64> {
    let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
}

/// The yt-dlp version, so `/about` never waits for a process to start.
#[derive(Default)]
pub struct YtdlpVersion {
    cached: Mutex<Option<(Instant, Option<String>)>>,
    refreshing: AtomicBool,
}

/// What `/about` shows for yt-dlp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum YtdlpLine {
    Known(String),
    /// Not asked yet (startup).
    Checking,
    Unavailable,
}

impl YtdlpVersion {
    pub fn line(&self) -> YtdlpLine {
        match self.cached.lock().ok().and_then(|c| c.clone()) {
            None => YtdlpLine::Checking,
            Some((_, Some(v))) => YtdlpLine::Known(v),
            Some((_, None)) => YtdlpLine::Unavailable,
        }
    }

    fn is_stale(&self) -> bool {
        self.cached
            .lock()
            .ok()
            .and_then(|c| c.as_ref().map(|(at, _)| at.elapsed() >= YTDLP_VERSION_TTL))
            .unwrap_or(true)
    }

    /// Ask yt-dlp for its version in the background, unless the cache is fresh or a check runs.
    pub fn refresh_if_stale(self: &Arc<Self>, sources: &Sources) {
        if !self.is_stale() || self.refreshing.swap(true, Ordering::AcqRel) {
            return;
        }
        let me = self.clone();
        let sources = sources.clone();
        tokio::spawn(async move {
            let v = sources.ytdlp().version().await;
            if let Ok(mut c) = me.cached.lock() {
                *c = Some((Instant::now(), v));
            }
            me.refreshing.store(false, Ordering::Release);
        });
    }
}

/// The `/about` card with live numbers.
pub async fn card(lynx: &Lynx) -> Message {
    lynx.ytdlp_version.refresh_if_stale(&lynx.sources);
    let main = lynx.bots.main();
    let players = lynx.players.all();
    let playing = players
        .iter()
        .filter(|p| p.snapshot().status == Status::Playing)
        .count();
    let avatar = main.is_ready().then(|| main.cache.current_user().face());
    let v = cards::About {
        version: version(),
        commit: commit(),
        avatar: avatar.as_deref(),
        uptime: uptime(),
        ping: main.gateway_latency().await,
        servers: lynx.bots.guilds_of(main.index).len(),
        bots: lynx.bots.bots().len(),
        voice_channels: players.len(),
        playing,
        memory_bytes: memory_bytes(),
        ytdlp: lynx.ytdlp_version.line(),
        invite_url: &lynx.main_invite(),
        support_url: lynx.config.support_url.as_deref(),
        website_url: lynx.config.landing_url.as_deref(),
    };
    cards::about(&v, &lynx.i18n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rss_from_proc_status() {
        let status = "Name:\tlynx\nVmPeak:\t  999 kB\nVmRSS:\t   87040 kB\nThreads:\t12\n";
        assert_eq!(parse_vm_rss(status), Some(87040 * 1024));
        assert_eq!(parse_vm_rss("Name:\tlynx\n"), None);
    }

    #[test]
    fn build_info() {
        assert!(!version().is_empty());
        if let Some(c) = commit() {
            assert_eq!(c.len(), 7);
            assert!(c.chars().all(|ch| ch.is_ascii_hexdigit()));
        }
        mark_start();
        assert!(uptime() < Duration::from_secs(24 * 3600));
    }

    #[test]
    fn ytdlp_line_states() {
        let v = YtdlpVersion::default();
        assert_eq!(v.line(), YtdlpLine::Checking);
        assert!(v.is_stale());
        *v.cached.lock().unwrap() = Some((Instant::now(), Some("2025.10.22".into())));
        assert_eq!(v.line(), YtdlpLine::Known("2025.10.22".into()));
        assert!(!v.is_stale());
        *v.cached.lock().unwrap() = Some((Instant::now(), None));
        assert_eq!(v.line(), YtdlpLine::Unavailable);
    }
}
