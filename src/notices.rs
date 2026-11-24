//! Operator notices (scheduled downtime, info, warnings) and the runtime flags set from the admin
//! panel (`src/api/admin`).
//!
//! The [`NoticeBoard`] keeps the notices and flags in memory (loaded from SQLite at startup, written
//! through by the admin API) so every Discord reply can read them synchronously. Users see the
//! soonest *visible* notice: active, `show_from <= now < ends_at`. Discord replies get a one-line
//! subtext with Discord timestamps (see [`discord_line`]); the Activity gets the same notices through
//! `/api/public`, `/api/notices` and a `notices` WebSocket message.

use std::{
    sync::{Arc, RwLock},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::{
    discord::ui::{escape, truncate},
    i18n::I18n,
    store::{now_secs, Store},
};

/// Longest title and message accepted (characters).
pub const MAX_TITLE: usize = 100;
pub const MAX_MESSAGE: usize = 500;
/// Characters of the message shown in a Discord reply (the panel shows it whole).
const DISCORD_MESSAGE_CHARS: usize = 300;
/// How often the board checks whether a notice started or stopped being visible.
const TICK: Duration = Duration::from_secs(15);
/// Notices that ended this long ago are deleted.
const KEEP_ENDED_SECS: i64 = 30 * 24 * 60 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Maintenance,
    Info,
    Warning,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Maintenance => "maintenance",
            Self::Info => "info",
            Self::Warning => "warning",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "maintenance" => Some(Self::Maintenance),
            "info" => Some(Self::Info),
            "warning" => Some(Self::Warning),
            _ => None,
        }
    }
}

/// A stored notice. Times are epoch seconds (UTC).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub id: i64,
    pub kind: Kind,
    pub title: String,
    pub message: String,
    pub starts_at: i64,
    pub ends_at: i64,
    /// When users start seeing it (before `starts_at` for an announcement).
    pub show_from: i64,
    pub active: bool,
    pub created_at: i64,
    pub updated_at: i64,
    /// Written and managed by the automatic update checker (`crate::updates`), not by hand.
    pub auto: bool,
}

/// A notice as created or edited (validated by the admin API).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewNotice {
    pub kind: Kind,
    pub title: String,
    pub message: String,
    pub starts_at: i64,
    pub ends_at: i64,
    pub show_from: i64,
    pub active: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    /// Announced, not started yet.
    Upcoming,
    /// Between `starts_at` and `ends_at`.
    Active,
}

impl Notice {
    /// Whether users see the notice at `now`, and how.
    pub fn phase(&self, now: i64) -> Option<Phase> {
        if !self.active || now < self.show_from || now >= self.ends_at {
            return None;
        }
        Some(if now >= self.starts_at {
            Phase::Active
        } else {
            Phase::Upcoming
        })
    }

    /// Expected duration in whole minutes (rounded up).
    pub fn duration_minutes(&self) -> i64 {
        ((self.ends_at - self.starts_at).max(0) + 59) / 60
    }

    /// What anyone may see (Activity side): no creation times, no inactive or future notices.
    pub fn public_json(&self, phase: Phase) -> Value {
        json!({
            "id": self.id,
            "kind": self.kind,
            "title": self.title,
            "message": self.message,
            "startsAt": self.starts_at * 1000,
            "endsAt": self.ends_at * 1000,
            "phase": phase,
        })
    }

    /// Full view for the admin panel.
    pub fn admin_json(&self, now: i64) -> Value {
        json!({
            "id": self.id,
            "kind": self.kind,
            "title": self.title,
            "message": self.message,
            "startsAt": self.starts_at * 1000,
            "endsAt": self.ends_at * 1000,
            "showFrom": self.show_from * 1000,
            "active": self.active,
            "createdAt": self.created_at * 1000,
            "updatedAt": self.updated_at * 1000,
            "auto": self.auto,
            "phase": self.phase(now),
            "ended": now >= self.ends_at,
        })
    }
}

/// The notices users see at `now`, soonest first (a running one before an announced one).
pub fn visible(list: &[Notice], now: i64) -> Vec<(&Notice, Phase)> {
    let mut out: Vec<(&Notice, Phase)> = list.iter().filter_map(|n| n.phase(now).map(|p| (n, p))).collect();
    out.sort_by_key(|(n, _)| (n.starts_at, n.id));
    out
}

/// The one notice shown in Discord replies.
pub fn select(list: &[Notice], now: i64) -> Option<(&Notice, Phase)> {
    visible(list, now).into_iter().next()
}

/// Runtime switches (admin panel > Tweaks). Persisted in `admin_flags`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Flags {
    /// New songs are refused (`/play`, panel adds); what is playing keeps playing.
    pub maintenance_mode: bool,
    /// The daily yt-dlp self-update is skipped.
    pub ytdlp_updates_paused: bool,
}

/// Notices and flags shared by the Discord layer, the panel API and the admin API. Cheap to clone.
#[derive(Clone)]
pub struct NoticeBoard(Arc<Inner>);

struct Inner {
    notices: RwLock<Vec<Notice>>,
    flags: RwLock<Flags>,
    /// Bumped whenever what users see may have changed.
    changed: watch::Sender<u64>,
}

impl Default for NoticeBoard {
    fn default() -> Self {
        Self(Arc::new(Inner {
            notices: RwLock::new(Vec::new()),
            flags: RwLock::new(Flags::default()),
            changed: watch::channel(0).0,
        }))
    }
}

impl NoticeBoard {
    /// Board filled from SQLite.
    pub async fn load(store: &Store) -> anyhow::Result<Self> {
        let board = Self::default();
        board.set_notices(store.notices().await?);
        board.set_flags(store.admin_flags().await?);
        Ok(board)
    }

    pub fn notices(&self) -> Vec<Notice> {
        self.0.notices.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    pub fn set_notices(&self, list: Vec<Notice>) {
        *self.0.notices.write().unwrap_or_else(|p| p.into_inner()) = list;
        self.bump();
    }

    pub fn flags(&self) -> Flags {
        *self.0.flags.read().unwrap_or_else(|p| p.into_inner())
    }

    pub fn set_flags(&self, f: Flags) {
        *self.0.flags.write().unwrap_or_else(|p| p.into_inner()) = f;
        self.bump();
    }

    pub fn maintenance_mode(&self) -> bool {
        self.flags().maintenance_mode
    }

    /// The notice shown at `now`.
    pub fn current(&self, now: i64) -> Option<(Notice, Phase)> {
        let list = self.0.notices.read().unwrap_or_else(|p| p.into_inner());
        select(&list, now).map(|(n, p)| (n.clone(), p))
    }

    /// `{ notices: [...], maintenance }` for the Activity (`/api/public`, `/api/notices`, WebSocket).
    pub fn public_json(&self, now: i64) -> Value {
        let list = self.0.notices.read().unwrap_or_else(|p| p.into_inner());
        let notices: Vec<Value> = visible(&list, now).into_iter().map(|(n, p)| n.public_json(p)).collect();
        json!({ "notices": notices, "maintenance": self.maintenance_mode() })
    }

    /// Visible ids and phases: changes when a notice appears, starts or ends.
    fn signature(&self, now: i64) -> Vec<(i64, Phase)> {
        let list = self.0.notices.read().unwrap_or_else(|p| p.into_inner());
        visible(&list, now).into_iter().map(|(n, p)| (n.id, p)).collect()
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.0.changed.subscribe()
    }

    fn bump(&self) {
        self.0.changed.send_modify(|v| *v = v.wrapping_add(1));
    }

    /// Re-announces the board when a notice becomes visible, starts or ends (time passing), and
    /// deletes notices that ended long ago. Ends on `stop`.
    pub fn spawn_ticker(&self, store: Store, stop: CancellationToken) -> tokio::task::JoinHandle<()> {
        let board = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(TICK);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut last = board.signature(now_secs());
            let mut last_prune = 0i64;
            loop {
                tokio::select! {
                    _ = stop.cancelled() => return,
                    _ = tick.tick() => {}
                }
                let now = now_secs();
                let sig = board.signature(now);
                if sig != last {
                    debug!("visible notices changed");
                    last = sig;
                    board.bump();
                }
                if now - last_prune >= 60 * 60 {
                    last_prune = now;
                    match store.prune_notices(now - KEEP_ENDED_SECS).await {
                        Ok(0) => {}
                        Ok(n) => {
                            debug!(removed = n, "old notices deleted");
                            if let Ok(list) = store.notices().await {
                                board.set_notices(list);
                            }
                        }
                        Err(e) => warn!(error = %e, "could not prune old notices"),
                    }
                }
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Discord rendering
// ---------------------------------------------------------------------------

/// One line of plain user text: control characters and line breaks become spaces, runs of
/// whitespace collapse, then it is cut to `max` characters and Discord markdown is escaped.
pub fn clean_text(s: &str, max: usize) -> String {
    let flat: String = s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let flat = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    escape(&truncate(&flat, max))
}

/// "10 minutes", "1 hour", "1 h 30 min".
pub fn duration_text(minutes: i64, i18n: &I18n) -> String {
    let m = minutes.max(1);
    let n = m.to_string();
    if m < 60 {
        let key = if m == 1 {
            "bot.announcement.duration.minute"
        } else {
            "bot.announcement.duration.minutes"
        };
        return i18n.t(key, &[("n", &n)]);
    }
    let (h, rest) = (m / 60, m % 60);
    if rest == 0 {
        let key = if h == 1 {
            "bot.announcement.duration.hour"
        } else {
            "bot.announcement.duration.hours"
        };
        return i18n.t(key, &[("n", &h.to_string())]);
    }
    i18n.t(
        "bot.announcement.duration.mixed",
        &[("h", &h.to_string()), ("m", &rest.to_string())],
    )
}

/// The notice line added to Discord replies (subtext). Times use Discord timestamp markup, so each
/// user reads them in their own time zone; the title and message are escaped.
pub fn discord_line(n: &Notice, phase: Phase, i18n: &I18n) -> String {
    let label = i18n.t(
        match (n.kind, phase) {
            (Kind::Maintenance, Phase::Upcoming) => "bot.announcement.label.maintenance",
            (Kind::Maintenance, Phase::Active) => "bot.announcement.label.maintenance_active",
            (Kind::Info, _) => "bot.announcement.label.info",
            (Kind::Warning, _) => "bot.announcement.label.warning",
        },
        &[],
    );
    let when = match phase {
        Phase::Upcoming => i18n.t(
            "bot.announcement.upcoming",
            &[
                ("start", &format!("<t:{}:F>", n.starts_at)),
                ("relative", &format!("<t:{}:R>", n.starts_at)),
                ("duration", &duration_text(n.duration_minutes(), i18n)),
            ],
        ),
        Phase::Active => i18n.t(
            "bot.announcement.active",
            &[
                ("end", &format!("<t:{}:t>", n.ends_at)),
                ("relative", &format!("<t:{}:R>", n.ends_at)),
            ],
        ),
    };
    let title = clean_text(&n.title, MAX_TITLE);
    let message = clean_text(&n.message, DISCORD_MESSAGE_CHARS);
    let text = match (title.is_empty(), message.is_empty()) {
        (false, false) => format!("**{title}** - {message}"),
        (false, true) => format!("**{title}**"),
        (true, false) => message,
        (true, true) => String::new(),
    };
    let line = if text.is_empty() {
        i18n.t("bot.announcement.line_bare", &[("label", &label), ("when", &when)])
    } else {
        i18n.t(
            "bot.announcement.line",
            &[("label", &label), ("when", &when), ("text", &text)],
        )
    };
    format!("-# {line}")
}

#[cfg(not(test))]
static GLOBAL: std::sync::OnceLock<(NoticeBoard, Arc<I18n>)> = std::sync::OnceLock::new();

#[cfg(test)]
thread_local! {
    static GLOBAL: std::cell::RefCell<Option<(NoticeBoard, Arc<I18n>)>> = const { std::cell::RefCell::new(None) };
}

/// Make the board the source of the notice line in every Discord message (call once in `main`).
/// Tests install per thread, so parallel tests never see each other's notices.
pub fn install(board: NoticeBoard, i18n: Arc<I18n>) {
    #[cfg(not(test))]
    {
        let _ = GLOBAL.set((board, i18n));
    }
    #[cfg(test)]
    GLOBAL.with(|g| *g.borrow_mut() = Some((board, i18n)));
}

#[cfg(test)]
pub fn uninstall() {
    GLOBAL.with(|g| *g.borrow_mut() = None);
}

/// The notice line for a Discord message sent now, if a notice is visible.
pub fn current_discord_line() -> Option<String> {
    let now = now_secs();
    #[cfg(not(test))]
    {
        let (board, i18n) = GLOBAL.get()?;
        board.current(now).map(|(n, p)| discord_line(&n, p, i18n))
    }
    #[cfg(test)]
    {
        GLOBAL.with(|g| {
            let g = g.borrow();
            let (board, i18n) = g.as_ref()?;
            board.current(now).map(|(n, p)| discord_line(&n, p, i18n))
        })
    }
}

#[cfg(test)]
pub(crate) mod test_util {
    use super::*;

    pub fn notice(id: i64, kind: Kind, starts_at: i64, minutes: i64, show_from: i64) -> Notice {
        Notice {
            id,
            kind,
            title: "Upgrade".into(),
            message: "New server".into(),
            starts_at,
            ends_at: starts_at + minutes * 60,
            show_from,
            active: true,
            created_at: 0,
            updated_at: 0,
            auto: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{test_util::notice, *};

    fn en() -> I18n {
        I18n::load("en").unwrap()
    }

    #[test]
    fn phases_follow_show_from_start_and_end() {
        let n = notice(1, Kind::Maintenance, 1_000, 10, 400);
        assert_eq!(n.phase(399), None, "not announced yet");
        assert_eq!(n.phase(400), Some(Phase::Upcoming));
        assert_eq!(n.phase(999), Some(Phase::Upcoming));
        assert_eq!(n.phase(1_000), Some(Phase::Active));
        assert_eq!(n.phase(1_599), Some(Phase::Active));
        assert_eq!(n.phase(1_600), None, "expired at the end time");
        let off = Notice {
            active: false,
            ..n.clone()
        };
        assert_eq!(off.phase(1_000), None, "inactive notices are hidden");
        assert_eq!(n.duration_minutes(), 10);
        assert_eq!(Notice { ends_at: 1_061, ..n }.duration_minutes(), 2, "rounded up");
    }

    #[test]
    fn selection_picks_the_soonest_visible_notice() {
        let list = vec![
            notice(1, Kind::Info, 5_000, 10, 0),            // upcoming, later
            notice(2, Kind::Maintenance, 2_000, 10, 0),     // upcoming, sooner
            notice(3, Kind::Warning, 100, 5, 0),            // expired at 400
            notice(4, Kind::Maintenance, 1_500, 10, 1_200), // not announced before 1200
        ];
        let (n, p) = select(&list, 1_000).unwrap();
        assert_eq!((n.id, p), (2, Phase::Upcoming));
        assert_eq!(
            visible(&list, 1_000).iter().map(|(n, _)| n.id).collect::<Vec<_>>(),
            vec![2, 1]
        );
        // Once announced, the earlier one wins; once started it is active.
        assert_eq!(select(&list, 1_300).unwrap().0.id, 4);
        assert_eq!(select(&list, 1_500).unwrap().1, Phase::Active);
        // Two running: the one that started first. Once it ends, the next; a running one beats an
        // announced one; after everything ends nothing is shown.
        assert_eq!(select(&list, 2_050).unwrap().0.id, 4);
        assert_eq!(select(&list, 2_150).unwrap(), (&list[1], Phase::Active));
        assert!(select(&list, 10_000).is_none());
        assert!(select(&[], 0).is_none());
        // Same start: the lower id first (stable).
        let tie = vec![notice(9, Kind::Info, 50, 1, 0), notice(8, Kind::Info, 50, 1, 0)];
        assert_eq!(select(&tie, 0).unwrap().0.id, 8);
    }

    #[test]
    fn board_public_json_hides_future_and_inactive_notices() {
        let board = NoticeBoard::default();
        let mut hidden = notice(2, Kind::Info, 9_000_000_000, 10, 8_000_000_000);
        hidden.title = "secret plan".into();
        let mut off = notice(3, Kind::Info, 0, 10, 0);
        off.active = false;
        let now = now_secs();
        let shown = notice(1, Kind::Maintenance, now + 3600, 10, now - 10);
        let rx = board.subscribe();
        board.set_notices(vec![hidden, off, shown]);
        assert!(rx.has_changed().unwrap());
        let v = board.public_json(now);
        let list = v["notices"].as_array().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["id"], 1);
        assert_eq!(list[0]["phase"], "upcoming");
        assert_eq!(list[0]["startsAt"], (now + 3600) * 1000);
        assert!(list[0].get("showFrom").is_none() && list[0].get("createdAt").is_none());
        assert!(!v.to_string().contains("secret plan"));
        assert_eq!(v["maintenance"], false);
        board.set_flags(Flags {
            maintenance_mode: true,
            ..Flags::default()
        });
        assert_eq!(board.public_json(now)["maintenance"], true);
    }

    #[test]
    fn durations() {
        let i = en();
        assert_eq!(duration_text(1, &i), "1 minute");
        assert_eq!(duration_text(10, &i), "10 minutes");
        assert_eq!(duration_text(60, &i), "1 hour");
        assert_eq!(duration_text(120, &i), "2 hours");
        assert_eq!(duration_text(90, &i), "1 h 30 min");
        assert_eq!(duration_text(0, &i), "1 minute");
        let es = I18n::load("es").unwrap();
        assert_eq!(duration_text(10, &es), "10 minutos");
    }

    #[test]
    fn discord_line_uses_timestamps_and_escapes_text() {
        let i = en();
        let n = notice(1, Kind::Maintenance, 1_700_000_000, 10, 0);
        assert_eq!(
            discord_line(&n, Phase::Upcoming, &i),
            "-# Scheduled downtime on <t:1700000000:F> (<t:1700000000:R>) for about 10 minutes: **Upgrade** - New server"
        );
        assert_eq!(
            discord_line(&n, Phase::Active, &i),
            "-# Maintenance in progress until <t:1700000600:t> (<t:1700000600:R>): **Upgrade** - New server"
        );

        let mut evil = notice(2, Kind::Warning, 1_700_000_000, 60, 0);
        evil.title = "**bold** @everyone".into();
        evil.message = "<@123> [x](https://e.vil) `code`\n# heading\r\n||spoiler|| <t:0:R>".into();
        let line = discord_line(&evil, Phase::Upcoming, &i);
        assert!(line.starts_with("-# Warning on <t:1700000000:F>"), "{line}");
        assert!(line.contains("\\*\\*bold\\*\\* \\@everyone"), "{line}");
        assert!(line.contains("\\<\\@123\\>"), "{line}");
        assert!(line.contains("\\[x\\]\\(https://e.vil\\)"), "{line}");
        assert!(line.contains("\\`code\\`"), "{line}");
        assert!(line.contains("\\# heading"), "{line}");
        assert!(line.contains("\\|\\|spoiler\\|\\|"), "{line}");
        assert!(
            line.contains("\\<t:0:R\\>"),
            "user text cannot inject a timestamp: {line}"
        );
        assert!(!line.contains('\n') && !line.contains('\r'), "one line only");

        let mut long = notice(3, Kind::Info, 1_700_000_000, 5, 0);
        long.title = String::new();
        long.message = "a".repeat(2_000);
        let line = discord_line(&long, Phase::Upcoming, &i);
        assert!(line.chars().count() < 450, "message is cut");
        assert!(line.starts_with("-# Notice on"), "{line}");

        let mut bare = long.clone();
        bare.message = "   ".into();
        assert_eq!(
            discord_line(&bare, Phase::Upcoming, &i),
            "-# Notice on <t:1700000000:F> (<t:1700000000:R>) for about 5 minutes."
        );

        let es = I18n::load("es").unwrap();
        assert!(discord_line(&n, Phase::Upcoming, &es).starts_with("-# Mantenimiento programado el <t:1700000000:F>"));
    }

    #[test]
    fn global_line_follows_the_installed_board() {
        uninstall();
        assert!(current_discord_line().is_none());
        let board = NoticeBoard::default();
        install(board.clone(), Arc::new(en()));
        assert!(current_discord_line().is_none(), "no notice");
        let now = now_secs();
        board.set_notices(vec![notice(1, Kind::Maintenance, now + 600, 10, now - 1)]);
        let line = current_discord_line().unwrap();
        assert!(line.starts_with("-# Scheduled downtime on <t:"), "{line}");
        uninstall();
    }
}
