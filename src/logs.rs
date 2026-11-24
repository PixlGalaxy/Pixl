//! Live log tee for the admin panel: a tracing layer that copies every event (after the
//! `LOG_LEVEL` filter) into a bounded in-memory ring and a broadcast channel.
//!
//! It never blocks the code that logs: formatting happens outside any lock, the ring lock is held
//! only to push one line, and `broadcast::send` never waits (a slow or stuck viewer just lags and
//! skips lines). It also counts warnings and errors for the admin activity summary.
//! Anything that looks like a Discord token, a session token, an `Authorization` / `Cookie` value
//! or the admin cookie is redacted before it is stored (none of them should be logged anyway).

use std::{
    collections::VecDeque,
    fmt::Write as _,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, LazyLock, Mutex,
    },
};

use serde::Serialize;
use tokio::sync::broadcast;
use tracing::{
    field::{Field, Visit},
    Event, Level, Subscriber,
};
use tracing_subscriber::{layer::Context, Layer};

use crate::stats::{self, Counter};

/// Lines kept for a viewer that just opened.
pub const RING: usize = 2000;
/// Lines a live viewer may fall behind before it skips ahead.
const CHANNEL: usize = 1024;
/// Longest stored line (characters).
const MAX_LINE: usize = 4000;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogLine {
    pub seq: u64,
    /// Epoch ms.
    pub ts: i64,
    pub level: &'static str,
    pub target: String,
    /// The message plus its fields (`key=value`), redacted.
    pub text: String,
}

pub struct LogHub {
    ring: Mutex<VecDeque<Arc<LogLine>>>,
    tx: broadcast::Sender<Arc<LogLine>>,
    seq: AtomicU64,
    cap: usize,
}

impl LogHub {
    pub fn new(cap: usize, channel: usize) -> Self {
        Self {
            ring: Mutex::new(VecDeque::with_capacity(cap.min(4096))),
            tx: broadcast::channel(channel.max(1)).0,
            seq: AtomicU64::new(0),
            cap: cap.max(1),
        }
    }

    pub fn push(&self, level: &'static str, target: &str, text: &str) {
        let line = Arc::new(LogLine {
            seq: self.seq.fetch_add(1, Ordering::Relaxed) + 1,
            ts: now_ms(),
            level,
            target: target.to_owned(),
            text: redact(&crate::discord::ui::truncate(text, MAX_LINE)),
        });
        {
            let mut ring = self.ring.lock().unwrap_or_else(|p| p.into_inner());
            if ring.len() >= self.cap {
                ring.pop_front();
            }
            ring.push_back(line.clone());
        }
        // Never waits: with no receiver it fails, a slow receiver lags.
        let _ = self.tx.send(line);
    }

    /// Lines newer than `after` still in the ring, oldest first.
    pub fn since(&self, after: u64) -> Vec<Arc<LogLine>> {
        let ring = self.ring.lock().unwrap_or_else(|p| p.into_inner());
        ring.iter().filter(|l| l.seq > after).cloned().collect()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<LogLine>> {
        self.tx.subscribe()
    }

    pub fn latest_seq(&self) -> u64 {
        self.seq.load(Ordering::Relaxed)
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

static GLOBAL: LazyLock<LogHub> = LazyLock::new(|| LogHub::new(RING, CHANNEL));

pub fn global() -> &'static LogHub {
    &GLOBAL
}

/// The tracing layer (add it after the env filter).
pub struct TeeLayer;

struct Fields {
    message: String,
    rest: String,
    /// Real target of an event forwarded from the `log` crate (e.g. symphonia), whose own
    /// metadata only says "log" and carries `log.target` / `log.file` / ... as fields.
    log_target: Option<String>,
}

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message.push_str(value);
        } else if field.name() == "log.target" {
            self.log_target = Some(value.to_owned());
        } else if field.name().starts_with("log.") {
            // log.module_path / log.file / log.line: noise in the viewer.
        } else {
            let _ = write!(self.rest, " {}={}", field.name(), value);
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.message, "{value:?}");
        } else if field.name().starts_with("log.") {
        } else {
            let _ = write!(self.rest, " {}={:?}", field.name(), value);
        }
    }
}

impl<S: Subscriber> Layer<S> for TeeLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        let level = match *meta.level() {
            Level::ERROR => {
                stats::count(Counter::Errors);
                "error"
            }
            Level::WARN => {
                stats::count(Counter::Warnings);
                "warn"
            }
            Level::INFO => "info",
            Level::DEBUG => "debug",
            Level::TRACE => "trace",
        };
        let mut f = Fields {
            message: String::new(),
            rest: String::new(),
            log_target: None,
        };
        event.record(&mut f);
        f.message.push_str(&f.rest);
        let target = f.log_target.as_deref().unwrap_or(meta.target());
        GLOBAL.push(level, target, f.message.trim_end());
    }
}

fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// Discord bot token shape: `<base64 id>.<6+ chars>.<27+ chars>`.
fn looks_like_discord_token(w: &str) -> bool {
    let parts: Vec<&str> = w.split('.').collect();
    parts.len() == 3
        && parts[0].len() >= 18
        && parts[1].len() >= 5
        && parts[2].len() >= 20
        && parts.iter().all(|p| p.chars().all(is_token_char))
}

/// Long hex runs (session tokens and their hashes are 64 hex characters).
fn looks_like_session_token(w: &str) -> bool {
    w.len() >= 40 && w.chars().all(|c| c.is_ascii_hexdigit())
}

/// Replace secrets that should never be logged with `[redacted]`.
pub fn redact(s: &str) -> String {
    // 1. Header-style secrets. Header values run to the end of the line (they may hold spaces,
    //    "Bot <token>", several cookies); a bearer token or the admin cookie to the next separator.
    let mut out = s.to_owned();
    for (marker, whole_value) in [
        ("authorization:", true),
        ("authorization=", true),
        ("set-cookie:", true),
        ("cookie:", true),
        ("cookie=", true),
        ("bearer ", false),
        ("lynx_admin=", false),
    ] {
        let mut from = 0;
        while let Some(pos) = out[from..].to_ascii_lowercase().find(marker).map(|p| p + from) {
            let start = pos + marker.len();
            let value_start = start + (out[start..].len() - out[start..].trim_start().len());
            let value_end = out[value_start..]
                .find(|c: char| {
                    if whole_value {
                        c == '\n' || c == '"'
                    } else {
                        c.is_whitespace() || c == '"' || c == ',' || c == ';'
                    }
                })
                .map(|e| value_start + e)
                .unwrap_or(out.len());
            if value_end > value_start && &out[value_start..value_end] != "[redacted]" {
                out.replace_range(value_start..value_end, "[redacted]");
            }
            from = (value_start + "[redacted]".len()).min(out.len());
        }
    }
    // 2. Token-shaped words anywhere.
    let mut result = String::with_capacity(out.len());
    let mut word = String::new();
    let flush = |word: &mut String, result: &mut String| {
        if looks_like_discord_token(word) || looks_like_session_token(word) {
            result.push_str("[redacted]");
        } else {
            result.push_str(word);
        }
        word.clear();
    };
    for c in out.chars() {
        if is_token_char(c) || c == '.' {
            word.push(c);
        } else {
            flush(&mut word, &mut result);
            result.push(c);
        }
    }
    flush(&mut word, &mut result);
    result
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    #[test]
    fn ring_is_bounded_and_ordered() {
        let hub = LogHub::new(3, 8);
        for i in 0..5 {
            hub.push("info", "lynx", &format!("line {i}"));
        }
        let lines = hub.since(0);
        assert_eq!(
            lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(),
            ["line 2", "line 3", "line 4"]
        );
        assert_eq!(hub.since(4).len(), 1);
        assert_eq!(hub.latest_seq(), 5);
    }

    #[test]
    fn a_stuck_viewer_never_blocks_writers() {
        let hub = LogHub::new(RING, 16);
        // A subscriber that never reads (a frozen log viewer).
        let mut stuck = hub.subscribe();
        let started = Instant::now();
        for i in 0..50_000 {
            hub.push("info", "lynx", &format!("busy line {i}"));
        }
        assert!(started.elapsed() < Duration::from_secs(5), "writers were slowed down");
        assert_eq!(hub.since(0).len(), RING, "the ring stays bounded");
        // The stuck one only finds out it lagged; it never held anybody up.
        assert!(matches!(
            stuck.try_recv(),
            Err(broadcast::error::TryRecvError::Lagged(_))
        ));
    }

    #[test]
    fn secrets_are_redacted() {
        let token = format!("{}.{}.{}", "M".repeat(24), "Gabcde", "x".repeat(27));
        let r = redact(&format!("connecting with {token} now"));
        assert_eq!(r, "connecting with [redacted] now");
        assert_eq!(redact("Authorization: Bearer abc.def"), "Authorization: [redacted]");
        assert_eq!(
            redact("headers: authorization=Bot xyz123"),
            "headers: authorization=[redacted]"
        );
        assert_eq!(redact("sent Bearer abc123 to x"), "sent Bearer [redacted] to x");
        assert_eq!(redact("cookie=lynx_admin=0123; a=1"), "cookie=[redacted]");
        assert_eq!(redact("got lynx_admin=0123; next"), "got lynx_admin=[redacted]; next");
        let session = "f".repeat(64);
        assert_eq!(redact(&format!("session {session} gone")), "session [redacted] gone");
        // Ordinary lines are untouched.
        let plain = "track start: match=- meta=120ms resolve=640ms total=1.2s guild=123456789012345678";
        assert_eq!(redact(plain), plain);
        assert_eq!(redact("yt-dlp updated to 2026.11.23"), "yt-dlp updated to 2026.11.23");
    }
}
