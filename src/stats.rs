//! Activity counters for the admin panel ("what happened in the last hour / 24 h").
//!
//! A ring of per-minute buckets (24 h) made only of atomics: the bot's hot paths (commands, panel
//! requests, track starts) count with a few relaxed atomic adds, never a lock, and the admin panel
//! reads them without blocking anyone. Nothing is persisted; a restart starts from zero.
//! Durations (yt-dlp resolve, time to first audio) go into a fixed log-scale histogram per minute,
//! so the average is exact and the p95 is an estimate within one bucket.

use std::{
    sync::{
        atomic::{AtomicI64, AtomicU64, Ordering::Relaxed},
        LazyLock,
    },
    time::Duration,
};

use serde_json::{json, Map, Value};

use crate::store::now_secs;

/// Minutes kept (24 h).
pub const MINUTES: usize = 24 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Counter {
    Commands,
    Buttons,
    Autocomplete,
    ApiRequests,
    TracksStarted,
    TracksFailed,
    Searches,
    /// Stream URL resolves that ran yt-dlp (cache misses), from the track start timings.
    Resolves,
    ResolvesCached,
    RateLimited,
    Warnings,
    Errors,
}

const COUNTERS: [(Counter, &str); 12] = [
    (Counter::Commands, "commands"),
    (Counter::Buttons, "buttons"),
    (Counter::Autocomplete, "autocomplete"),
    (Counter::ApiRequests, "apiRequests"),
    (Counter::TracksStarted, "tracksStarted"),
    (Counter::TracksFailed, "tracksFailed"),
    (Counter::Searches, "searches"),
    (Counter::Resolves, "resolves"),
    (Counter::ResolvesCached, "resolvesCached"),
    (Counter::RateLimited, "rateLimited"),
    (Counter::Warnings, "warnings"),
    (Counter::Errors, "errors"),
];

/// Slash commands counted by name (anything else is "other").
pub const COMMANDS: [&str; 20] = [
    "play",
    "pause",
    "resume",
    "skip",
    "stop",
    "queue",
    "nowplaying",
    "remove",
    "move",
    "skipto",
    "shuffle",
    "loop",
    "volume",
    "lyrics",
    "ai",
    "invite",
    "about",
    "panel",
    "launch",
    "other",
];

/// Button kinds.
pub const BUTTONS: [&str; 5] = ["now_playing", "queue_page", "replay", "radio", "other"];

/// Panel API route groups (first path segment after `/api`, or the top-level route).
pub const API_GROUPS: [&str; 22] = [
    "public",
    "notices",
    "auth",
    "player",
    "queue",
    "tracks",
    "playlists",
    "ai",
    "jam",
    "search",
    "lyrics",
    "settings",
    "djs",
    "members",
    "guild",
    "me",
    "ws",
    "cdn",
    "img",
    "health",
    "healthz",
    "other",
];

/// Upper edges (ms) of the duration histogram; the last bin is open.
const EDGES_MS: [u64; 18] = [
    50, 100, 200, 300, 500, 750, 1_000, 1_500, 2_000, 3_000, 4_000, 5_000, 7_500, 10_000, 15_000, 20_000, 30_000,
    60_000,
];
const BINS: usize = EDGES_MS.len() + 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Timing {
    /// yt-dlp stream resolve (cache misses only).
    Resolve,
    /// Track asked to play -> first audio.
    Start,
}

struct Hist {
    bins: [AtomicU64; BINS],
    sum_ms: AtomicU64,
}

impl Hist {
    fn new() -> Self {
        Self {
            bins: std::array::from_fn(|_| AtomicU64::new(0)),
            sum_ms: AtomicU64::new(0),
        }
    }

    fn add(&self, ms: u64) {
        let i = EDGES_MS.iter().position(|e| ms <= *e).unwrap_or(BINS - 1);
        self.bins[i].fetch_add(1, Relaxed);
        self.sum_ms.fetch_add(ms, Relaxed);
    }

    fn clear(&self) {
        for b in &self.bins {
            b.store(0, Relaxed);
        }
        self.sum_ms.store(0, Relaxed);
    }
}

struct Bucket {
    minute: AtomicI64,
    counters: [AtomicU64; COUNTERS.len()],
    commands: [AtomicU64; COMMANDS.len()],
    buttons: [AtomicU64; BUTTONS.len()],
    api: [AtomicU64; API_GROUPS.len()],
    resolve: Hist,
    start: Hist,
}

impl Bucket {
    fn new() -> Self {
        Self {
            minute: AtomicI64::new(-1),
            counters: std::array::from_fn(|_| AtomicU64::new(0)),
            commands: std::array::from_fn(|_| AtomicU64::new(0)),
            buttons: std::array::from_fn(|_| AtomicU64::new(0)),
            api: std::array::from_fn(|_| AtomicU64::new(0)),
            resolve: Hist::new(),
            start: Hist::new(),
        }
    }

    fn clear(&self) {
        for a in self
            .counters
            .iter()
            .chain(&self.commands)
            .chain(&self.buttons)
            .chain(&self.api)
        {
            a.store(0, Relaxed);
        }
        self.resolve.clear();
        self.start.clear();
    }
}

/// The counters. Use the global through the free functions; tests make their own.
pub struct Stats {
    buckets: Box<[Bucket]>,
}

impl Default for Stats {
    fn default() -> Self {
        Self {
            buckets: (0..MINUTES).map(|_| Bucket::new()).collect(),
        }
    }
}

fn index_of<const N: usize>(list: &[&str; N], name: &str) -> usize {
    list.iter().position(|x| *x == name).unwrap_or(N - 1)
}

/// Summary of one duration histogram.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimingSummary {
    pub count: u64,
    pub avg_ms: Option<u64>,
    pub p95_ms: Option<u64>,
}

impl Stats {
    /// The bucket of `minute`, reset first when it still holds an older minute. A reset that races
    /// with an increment may lose that one count: fine for statistics, and it never blocks.
    fn bucket(&self, minute: i64) -> &Bucket {
        let b = &self.buckets[minute.rem_euclid(MINUTES as i64) as usize];
        let cur = b.minute.load(Relaxed);
        if cur != minute && cur < minute && b.minute.compare_exchange(cur, minute, Relaxed, Relaxed).is_ok() {
            b.clear();
        }
        b
    }

    /// The bucket of `minute` if it holds that minute (reading never resets).
    fn read(&self, minute: i64) -> Option<&Bucket> {
        let b = &self.buckets[minute.rem_euclid(MINUTES as i64) as usize];
        (b.minute.load(Relaxed) == minute).then_some(b)
    }

    pub fn count_at(&self, minute: i64, c: Counter) {
        let i = COUNTERS.iter().position(|(x, _)| *x == c).unwrap_or(0);
        self.bucket(minute).counters[i].fetch_add(1, Relaxed);
    }

    pub fn command_at(&self, minute: i64, name: &str) {
        let b = self.bucket(minute);
        b.commands[index_of(&COMMANDS, name)].fetch_add(1, Relaxed);
        b.counters[0].fetch_add(1, Relaxed);
    }

    pub fn button_at(&self, minute: i64, kind: &str) {
        let b = self.bucket(minute);
        b.buttons[index_of(&BUTTONS, kind)].fetch_add(1, Relaxed);
        b.counters[1].fetch_add(1, Relaxed);
    }

    pub fn api_at(&self, minute: i64, group: &str) {
        let b = self.bucket(minute);
        b.api[index_of(&API_GROUPS, group)].fetch_add(1, Relaxed);
        b.counters[3].fetch_add(1, Relaxed);
    }

    pub fn timing_at(&self, minute: i64, t: Timing, d: Duration) {
        let b = self.bucket(minute);
        let ms = u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
        match t {
            Timing::Resolve => b.resolve.add(ms),
            Timing::Start => b.start.add(ms),
        }
    }

    fn timing_summary(&self, minutes: impl Iterator<Item = i64>, t: Timing) -> TimingSummary {
        let mut bins = [0u64; BINS];
        let mut sum = 0u64;
        for m in minutes {
            if let Some(b) = self.read(m) {
                let h = match t {
                    Timing::Resolve => &b.resolve,
                    Timing::Start => &b.start,
                };
                for (acc, x) in bins.iter_mut().zip(&h.bins) {
                    *acc += x.load(Relaxed);
                }
                sum += h.sum_ms.load(Relaxed);
            }
        }
        let count: u64 = bins.iter().sum();
        if count == 0 {
            return TimingSummary {
                count,
                avg_ms: None,
                p95_ms: None,
            };
        }
        // p95: the bin holding the 95th percentile, interpolated linearly inside it.
        let target = (count as f64 * 0.95).ceil() as u64;
        let mut seen = 0u64;
        let mut p95 = *EDGES_MS.last().unwrap_or(&0);
        for (i, n) in bins.iter().enumerate() {
            if seen + n >= target && *n > 0 {
                let lo = if i == 0 { 0 } else { EDGES_MS[i - 1] };
                let hi = EDGES_MS.get(i).copied().unwrap_or(lo * 2);
                let frac = (target - seen) as f64 / *n as f64;
                p95 = lo + ((hi - lo) as f64 * frac).round() as u64;
                break;
            }
            seen += n;
        }
        TimingSummary {
            count,
            avg_ms: Some(sum / count),
            p95_ms: Some(p95),
        }
    }

    /// Totals and a per-step series over the `window` minutes ending at `now_minute` (included),
    /// with points of `step` minutes each.
    pub fn summary_at(&self, now_minute: i64, window: usize, step: usize) -> Value {
        let step = step.max(1);
        let first = now_minute - window as i64 + 1;
        let minutes = || first..=now_minute;
        let sum = |f: &dyn Fn(&Bucket) -> u64| -> u64 { minutes().filter_map(|m| self.read(m)).map(f).sum() };

        let mut totals = Map::new();
        let mut series = Map::new();
        for (i, (_, name)) in COUNTERS.iter().enumerate() {
            totals.insert((*name).into(), json!(sum(&|b| b.counters[i].load(Relaxed))));
            let points: Vec<u64> = (0..window.div_ceil(step))
                .map(|p| {
                    let start = first + (p * step) as i64;
                    (start..(start + step as i64).min(now_minute + 1))
                        .filter_map(|m| self.read(m))
                        .map(|b| b.counters[i].load(Relaxed))
                        .sum()
                })
                .collect();
            series.insert((*name).into(), json!(points));
        }
        let named = |list: &[&str], get: &dyn Fn(&Bucket, usize) -> u64| -> Value {
            let mut rows: Vec<(String, u64)> = list
                .iter()
                .enumerate()
                .map(|(i, n)| ((*n).to_owned(), sum(&|b| get(b, i))))
                .filter(|(_, n)| *n > 0)
                .collect();
            rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            Value::Array(
                rows.into_iter()
                    .map(|(name, count)| json!({ "name": name, "count": count }))
                    .collect(),
            )
        };
        let timing = |t| {
            let s = self.timing_summary(minutes(), t);
            json!({ "count": s.count, "avgMs": s.avg_ms, "p95Ms": s.p95_ms })
        };
        json!({
            "windowMinutes": window,
            "stepMinutes": step,
            "endMinute": now_minute,
            "totals": totals,
            "series": series,
            "commands": named(&COMMANDS, &|b, i| b.commands[i].load(Relaxed)),
            "buttons": named(&BUTTONS, &|b, i| b.buttons[i].load(Relaxed)),
            "api": named(&API_GROUPS, &|b, i| b.api[i].load(Relaxed)),
            "resolve": timing(Timing::Resolve),
            "start": timing(Timing::Start),
        })
    }
}

static GLOBAL: LazyLock<Stats> = LazyLock::new(Stats::default);

fn minute_now() -> i64 {
    now_secs().div_euclid(60)
}

pub fn count(c: Counter) {
    GLOBAL.count_at(minute_now(), c);
}

pub fn command(name: &str) {
    GLOBAL.command_at(minute_now(), name);
}

pub fn button(kind: &str) {
    GLOBAL.button_at(minute_now(), kind);
}

pub fn api(group: &str) {
    GLOBAL.api_at(minute_now(), group);
}

pub fn timing(t: Timing, d: Duration) {
    GLOBAL.timing_at(minute_now(), t, d);
}

/// Last hour (per minute) and last 24 h (per 15 minutes).
pub fn summary() -> Value {
    let now = minute_now();
    json!({ "hour": GLOBAL.summary_at(now, 60, 1), "day": GLOBAL.summary_at(now, MINUTES, 15) })
}

/// The API route group of a matched route path (`/api/player/pause` -> `player`).
pub fn api_group(path: &str) -> &'static str {
    let mut parts = path.trim_start_matches('/').split('/');
    let first = parts.next().unwrap_or("");
    let name = if first == "api" {
        parts.next().unwrap_or("")
    } else {
        first
    };
    API_GROUPS.iter().find(|g| **g == name).copied().unwrap_or("other")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_land_in_their_minute_and_old_minutes_drop_out() {
        let s = Stats::default();
        s.command_at(100, "play");
        s.command_at(100, "play");
        s.command_at(101, "queue");
        s.command_at(101, "dance");
        s.count_at(101, Counter::TracksStarted);
        s.api_at(101, "player");
        s.button_at(101, "now_playing");
        let h = s.summary_at(101, 60, 1);
        assert_eq!(h["totals"]["commands"], 4);
        assert_eq!(h["totals"]["tracksStarted"], 1);
        assert_eq!(h["totals"]["apiRequests"], 1);
        assert_eq!(h["totals"]["buttons"], 1);
        assert_eq!(
            h["commands"],
            json!([{ "name": "play", "count": 2 }, { "name": "other", "count": 1 }, { "name": "queue", "count": 1 }])
        );
        let series = h["series"]["commands"].as_array().unwrap();
        assert_eq!(series.len(), 60);
        assert_eq!(series[58], 2);
        assert_eq!(series[59], 2);
        // An hour later minute 100 is outside the window.
        assert_eq!(s.summary_at(160, 60, 1)["totals"]["commands"], 2);
        assert_eq!(s.summary_at(161, 60, 1)["totals"]["commands"], 0);
        // A full day later the bucket is reused and starts from zero.
        s.command_at(100 + MINUTES as i64, "stop");
        let d = s.summary_at(101 + MINUTES as i64, MINUTES, 15);
        assert_eq!(d["commands"], json!([{ "name": "stop", "count": 1 }]));
        assert_eq!(d["series"]["commands"].as_array().unwrap().len(), 96);
    }

    #[test]
    fn timings_average_and_p95() {
        let s = Stats::default();
        assert_eq!(
            s.summary_at(5, 60, 1)["resolve"],
            json!({ "count": 0, "avgMs": null, "p95Ms": null })
        );
        for _ in 0..95 {
            s.timing_at(5, Timing::Resolve, Duration::from_millis(400));
        }
        for _ in 0..5 {
            s.timing_at(5, Timing::Resolve, Duration::from_millis(9_000));
        }
        let r = &s.summary_at(5, 60, 1)["resolve"];
        assert_eq!(r["count"], 100);
        assert_eq!(r["avgMs"], (95 * 400 + 5 * 9_000) / 100);
        let p95 = r["p95Ms"].as_u64().unwrap();
        assert!((300..=500).contains(&p95), "p95 in the 300-500 ms bin: {p95}");
    }

    #[test]
    fn api_groups_are_bounded() {
        assert_eq!(api_group("/api/player/pause"), "player");
        assert_eq!(api_group("/api/playlists/{id}/tracks"), "playlists");
        assert_eq!(api_group("/ws"), "ws");
        assert_eq!(api_group("/cdn/{*path}"), "cdn");
        assert_eq!(api_group("/api/whatever"), "other");
        assert_eq!(api_group("/"), "other");
    }

    #[test]
    fn concurrent_counting_never_blocks() {
        let s = std::sync::Arc::new(Stats::default());
        // The first count of a minute resets its bucket; after that counting is pure atomic adds.
        s.count_at(7, Counter::ApiRequests);
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let s = s.clone();
                std::thread::spawn(move || {
                    for _ in 0..10_000 {
                        s.count_at(7, Counter::ApiRequests);
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(s.summary_at(7, 60, 1)["totals"]["apiRequests"], 80_001);
    }
}
