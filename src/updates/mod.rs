//! Automatic "scheduled update" notices.
//!
//! The image runs on Unraid with the container auto-update plugin, which pulls a newer image at a
//! fixed time (every day or on one weekday). When the published image ([`ghcr`]) was built from a
//! different commit than the one running (`LYNX_BUILD_COMMIT`), the update will happen at the next
//! window ([`schedule`]), so one automatic maintenance notice is kept for it: it starts at the
//! window, lasts the expected duration and is shown from the lead time before. When a newer image
//! appears the same notice is kept; when the window passes and the bot still runs the old commit
//! (Unraid did not update), the notice moves to the next window; once the running commit matches
//! the published one (the container restarted on the new image) the notice is removed.
//!
//! Runs as one background task on the bot's runtime: every request has a timeout, a check never
//! blocks anything else, and failures are logged as warnings (no secrets) and shown in the admin
//! panel. Settings live in `admin_flags` (`auto_update`); the notice is an `admin_notices` row
//! tagged `auto`, so the checker never touches notices made by hand.

pub mod ghcr;
pub mod schedule;

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, RwLock,
    },
    time::Duration,
};

use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::{
    notices::{Kind, NewNotice, Notice, NoticeBoard, MAX_MESSAGE, MAX_TITLE},
    store::{now_secs, Store},
};
use ghcr::ImageRef;
use schedule::{Day, Frequency, Schedule};

pub const DEFAULT_IMAGE: &str = "ghcr.io/pixlgalaxy/lynx:lynx";
pub const MIN_INTERVAL_MINUTES: u32 = 10;
pub const MAX_INTERVAL_MINUTES: u32 = 24 * 60;
pub const MAX_LEAD_HOURS: u32 = 14 * 24;
pub const MAX_DURATION_MINUTES: u32 = 24 * 60;
/// The placeholder replaced by the expected duration in the templates.
pub const MINUTES_PLACEHOLDER: &str = "{minutes}";
/// First check after startup (lets the bot settle; after an update this removes the notice).
const FIRST_CHECK: Duration = Duration::from_secs(30);
/// A whole check (token, manifests, config) gives up after this.
const CHECK_TIMEOUT: Duration = Duration::from_secs(90);
const MAX_ERROR_CHARS: usize = 300;

/// Title and message of the notice in one language.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Template {
    pub title: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Templates {
    pub en: Template,
    pub es: Template,
}

impl Default for Templates {
    fn default() -> Self {
        Self {
            en: Template {
                title: "Scheduled update".into(),
                message: "Lynx will update to a new version. It takes about {minutes} minutes.".into(),
            },
            es: Template {
                title: "Actualización programada".into(),
                message: "Lynx se actualizará a una versión nueva. Tomará unos {minutes} minutos.".into(),
            },
        }
    }
}

impl Templates {
    /// The one for the bot's language (`BOT_LOCALE`).
    pub fn for_locale(&self, locale: &str) -> &Template {
        if locale.trim().to_ascii_lowercase().starts_with("es") {
            &self.es
        } else {
            &self.en
        }
    }
}

/// Settings of the automatic update notices (admin panel > Notices).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
pub struct Settings {
    pub enabled: bool,
    /// `ghcr.io/<owner>/<name>:<tag>`, the image Unraid pulls.
    pub image: String,
    pub frequency: Frequency,
    /// Weekly schedules only.
    pub weekday: Day,
    /// `HH:MM`, in `timezone`.
    pub time: String,
    /// IANA name, e.g. `America/Mexico_City`.
    pub timezone: String,
    /// How long before the window users start seeing the notice.
    pub lead_hours: u32,
    pub duration_minutes: u32,
    pub check_interval_minutes: u32,
    pub templates: Templates,
}

/// `TZ` when it is a valid IANA name, else UTC.
pub fn default_timezone() -> String {
    std::env::var("TZ")
        .ok()
        .filter(|tz| schedule::parse_tz(tz).is_some())
        .map(|tz| tz.trim().to_owned())
        .unwrap_or_else(|| "UTC".into())
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            image: DEFAULT_IMAGE.into(),
            frequency: Frequency::Daily,
            weekday: Day::Sun,
            time: "04:00".into(),
            timezone: default_timezone(),
            lead_hours: 24,
            duration_minutes: 5,
            check_interval_minutes: 60,
            templates: Templates::default(),
        }
    }
}

/// Settings that passed [`Settings::validate`].
#[derive(Debug, Clone)]
pub struct Valid {
    pub settings: Settings,
    pub image: ImageRef,
    pub schedule: Schedule,
}

fn check_template(t: &Template, lang: &str) -> Result<Template, String> {
    let title = t.title.trim().to_owned();
    let message = t.message.trim().to_owned();
    // The placeholder becomes at most 4 digits, so a template within the limits renders within them.
    if title.chars().count() > MAX_TITLE {
        return Err(format!("the {lang} title is longer than {MAX_TITLE} characters"));
    }
    if message.chars().count() > MAX_MESSAGE {
        return Err(format!("the {lang} message is longer than {MAX_MESSAGE} characters"));
    }
    if title.is_empty() && message.is_empty() {
        return Err(format!("add a title or a message in {lang}"));
    }
    Ok(Template { title, message })
}

impl Settings {
    /// Check everything (also when disabled, so what is saved always works when turned on).
    pub fn validate(mut self) -> Result<Valid, String> {
        let image = ImageRef::parse(&self.image).map_err(String::from)?;
        self.image = image.to_string();
        let time = schedule::parse_time(&self.time).ok_or("the time must be HH:MM (24 hours)")?;
        self.time = time.format("%H:%M").to_string();
        let tz = schedule::parse_tz(&self.timezone)
            .ok_or("unknown time zone, use an IANA name like America/Mexico_City or UTC")?;
        self.timezone = tz.name().to_owned();
        if !(1..=MAX_LEAD_HOURS).contains(&self.lead_hours) {
            return Err(format!("the lead time must be 1 to {MAX_LEAD_HOURS} hours"));
        }
        if !(1..=MAX_DURATION_MINUTES).contains(&self.duration_minutes) {
            return Err(format!("the duration must be 1 to {MAX_DURATION_MINUTES} minutes"));
        }
        if !(MIN_INTERVAL_MINUTES..=MAX_INTERVAL_MINUTES).contains(&self.check_interval_minutes) {
            return Err(format!(
                "the check interval must be {MIN_INTERVAL_MINUTES} to {MAX_INTERVAL_MINUTES} minutes"
            ));
        }
        self.templates = Templates {
            en: check_template(&self.templates.en, "English")?,
            es: check_template(&self.templates.es, "Spanish")?,
        };
        let schedule = Schedule {
            frequency: self.frequency,
            day: self.weekday,
            time,
            tz,
        };
        Ok(Valid {
            settings: self,
            image,
            schedule,
        })
    }

    /// Saved settings, or the defaults when there are none or they no longer validate.
    pub fn from_saved(raw: Option<&str>) -> Valid {
        let parsed = raw.map(|r| {
            serde_json::from_str::<Settings>(r)
                .map_err(|e| e.to_string())
                .and_then(Settings::validate)
        });
        match parsed {
            Some(Ok(v)) => v,
            Some(Err(e)) => {
                warn!(error = %e, "saved automatic update notice settings are invalid; using the defaults");
                Self::default().validate().expect("the defaults are valid")
            }
            None => Self::default().validate().expect("the defaults are valid"),
        }
    }
}

fn render(t: &Template, minutes: u32) -> (String, String) {
    let m = minutes.to_string();
    (
        t.title.replace(MINUTES_PLACEHOLDER, &m),
        t.message.replace(MINUTES_PLACEHOLDER, &m),
    )
}

/// What the checker does with its notice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Nothing to change.
    Keep,
    /// Remove the automatic notice (up to date, disabled, unknown build).
    Remove,
    /// Create the automatic notice or move/update it.
    Upsert(NewNotice),
}

/// Inputs of [`decide`].
pub struct Facts<'a> {
    pub enabled: bool,
    /// `LYNX_BUILD_COMMIT`; None for dev builds.
    pub running: Option<&'a str>,
    /// Commit of the published image (from a successful check).
    pub published: &'a str,
    pub valid: &'a Valid,
    /// `BOT_LOCALE`.
    pub locale: &'a str,
    /// The current automatic notice, if any.
    pub existing: Option<&'a Notice>,
    pub now: i64,
}

/// The pure decision of one check (see the module docs).
pub fn decide(f: &Facts) -> Decision {
    let remove = || {
        if f.existing.is_some() {
            Decision::Remove
        } else {
            Decision::Keep
        }
    };
    let Some(running) = f.running else {
        return remove();
    };
    if !f.enabled || ghcr::same_commit(running, f.published) {
        return remove();
    }
    let s = &f.valid.settings;
    // An update in progress keeps its window; otherwise the next one strictly after now (a window
    // that passed without the update rolls to the next one).
    let start = match f.existing {
        Some(e) if e.starts_at <= f.now && f.now < e.ends_at => e.starts_at,
        _ => match schedule::next_window(&f.valid.schedule, f.now) {
            Some(w) => w,
            None => return Decision::Keep,
        },
    };
    let ends_at = start + i64::from(s.duration_minutes) * 60;
    let wanted_from = f.now.max(start - i64::from(s.lead_hours) * 3600);
    // Already on screen for this window: leave show_from alone (no rewrite every check).
    let show_from = match f.existing {
        Some(e) if e.starts_at == start && e.show_from <= f.now => e.show_from,
        _ => wanted_from,
    };
    let (title, message) = render(s.templates.for_locale(f.locale), s.duration_minutes);
    let wanted = NewNotice {
        kind: Kind::Maintenance,
        title,
        message,
        starts_at: start,
        ends_at,
        show_from,
        active: true,
    };
    match f.existing {
        Some(e)
            if e.kind == wanted.kind
                && e.title == wanted.title
                && e.message == wanted.message
                && e.starts_at == wanted.starts_at
                && e.ends_at == wanted.ends_at
                && e.show_from == wanted.show_from
                && e.active =>
        {
            Decision::Keep
        }
        _ => Decision::Upsert(wanted),
    }
}

/// What the admin panel shows about the checker. Times are epoch ms.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub checking: bool,
    /// Short commit of the published image (last successful check).
    pub published_commit: Option<String>,
    /// Last check attempt, and the last one that worked.
    pub checked_at: Option<i64>,
    pub succeeded_at: Option<i64>,
    /// Why the last check failed (cleared by a successful one).
    pub last_error: Option<String>,
    /// Running and published commits differ (None until a check worked or for dev builds).
    pub update_available: Option<bool>,
}

/// Handle on the checker: its settings, status and wake-up. Cheap to clone.
#[derive(Clone, Default)]
pub struct UpdateChecker(Arc<Inner>);

struct Inner {
    settings: RwLock<Valid>,
    status: RwLock<Status>,
    wake: Notify,
    /// A check was asked for from the admin panel: query the registry even when disabled.
    forced: AtomicBool,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            settings: RwLock::new(Settings::from_saved(None)),
            status: RwLock::new(Status::default()),
            wake: Notify::new(),
            forced: AtomicBool::new(false),
        }
    }
}

/// `LYNX_BUILD_COMMIT`, when known.
pub fn running_commit() -> Option<&'static str> {
    crate::discord::about::commit()
}

impl UpdateChecker {
    /// Checker with the saved settings (the defaults when the database cannot be read).
    pub async fn load(store: &Store) -> Self {
        let raw = match store.auto_update_settings().await {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, "could not read the automatic update notice settings");
                None
            }
        };
        let checker = Self::default();
        *checker.0.settings.write().unwrap_or_else(|p| p.into_inner()) = Settings::from_saved(raw.as_deref());
        checker
    }

    pub fn settings(&self) -> Valid {
        self.0.settings.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Use new (already saved) settings and re-check right away.
    pub fn set_settings(&self, v: Valid) {
        *self.0.settings.write().unwrap_or_else(|p| p.into_inner()) = v;
        self.0.wake.notify_one();
    }

    pub fn status(&self) -> Status {
        self.0.status.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn update_status(&self, f: impl FnOnce(&mut Status)) {
        f(&mut self.0.status.write().unwrap_or_else(|p| p.into_inner()));
    }

    /// Ask for a registry check now (admin panel), also when the notices are off.
    pub fn check_now(&self) {
        self.0.forced.store(true, Ordering::SeqCst);
        self.update_status(|s| s.checking = true);
        self.0.wake.notify_one();
    }

    /// The next update window of the current settings (epoch seconds).
    pub fn next_window(&self, now: i64) -> Option<i64> {
        schedule::next_window(&self.settings().schedule, now)
    }

    /// The background task. Ends on `stop`.
    pub fn spawn(
        &self,
        store: Store,
        board: NoticeBoard,
        locale: String,
        secret: Option<String>,
        stop: CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        let me = self.clone();
        tokio::spawn(async move {
            let http = match ghcr::client() {
                Ok(c) => c,
                Err(e) => {
                    warn!(error = %e, "automatic update notices are unavailable: no HTTP client");
                    return;
                }
            };
            let run = Runner {
                checker: me.clone(),
                store,
                board,
                locale,
                secret,
                http,
            };
            let mut wait = FIRST_CHECK;
            loop {
                tokio::select! {
                    _ = stop.cancelled() => return,
                    _ = tokio::time::sleep(wait) => {}
                    _ = me.0.wake.notified() => {}
                }
                let forced = me.0.forced.swap(false, Ordering::SeqCst);
                if tokio::time::timeout(CHECK_TIMEOUT, run.once(forced)).await.is_err() {
                    warn!("automatic update check timed out");
                    me.update_status(|s| {
                        s.checked_at = Some(now_secs() * 1000);
                        s.last_error = Some("the check timed out".into());
                    });
                }
                me.update_status(|s| s.checking = false);
                wait = run.next_wait().await;
            }
        })
    }
}

struct Runner {
    checker: UpdateChecker,
    store: Store,
    board: NoticeBoard,
    locale: String,
    secret: Option<String>,
    http: reqwest::Client,
}

impl Runner {
    /// Until the next interval, or shortly after the current notice ends (to roll it when the
    /// update did not happen), whichever comes first.
    async fn next_wait(&self) -> Duration {
        let interval = Duration::from_secs(u64::from(self.checker.settings().settings.check_interval_minutes) * 60);
        let now = now_secs();
        let after_end = match self.store.auto_notice().await {
            Ok(Some(n)) if n.ends_at + 60 > now => Some(Duration::from_secs((n.ends_at + 60 - now) as u64)),
            _ => None,
        };
        after_end.map_or(interval, |d| d.min(interval))
    }

    async fn once(&self, forced: bool) {
        let valid = self.checker.settings();
        let enabled = valid.settings.enabled;
        let running = running_commit();
        if !enabled && !forced {
            self.apply_removal_only().await;
            return;
        }
        let now = now_secs();
        self.checker.update_status(|s| s.checking = true);
        let published = match ghcr::published_revision(&self.http, &valid.image, self.secret.as_deref()).await {
            Ok(c) => c,
            Err(e) => {
                let msg: String = format!("{e:#}").chars().take(MAX_ERROR_CHARS).collect();
                warn!(image = %valid.image, error = %msg, "automatic update check failed");
                self.checker.update_status(|s| {
                    s.checked_at = Some(now * 1000);
                    s.last_error = Some(msg);
                });
                if !enabled {
                    self.apply_removal_only().await;
                }
                return;
            }
        };
        debug!(image = %valid.image, published = %ghcr::short(&published), running = running.unwrap_or("unknown"), "update check");
        self.checker.update_status(|s| {
            s.checked_at = Some(now * 1000);
            s.succeeded_at = Some(now * 1000);
            s.last_error = None;
            s.published_commit = Some(ghcr::short(&published));
            s.update_available = running.map(|r| !ghcr::same_commit(r, &published));
        });
        let existing = match self.store.auto_notice().await {
            Ok(n) => n,
            Err(e) => {
                warn!(error = %e, "automatic update notice: could not read the database");
                return;
            }
        };
        let decision = decide(&Facts {
            enabled,
            running,
            published: &published,
            valid: &valid,
            locale: &self.locale,
            existing: existing.as_ref(),
            now,
        });
        self.apply(decision, &published).await;
    }

    /// Disabled: only make sure no automatic notice is left.
    async fn apply_removal_only(&self) {
        match self.store.auto_notice().await {
            Ok(Some(_)) => self.apply(Decision::Remove, "").await,
            Ok(None) => {}
            Err(e) => warn!(error = %e, "automatic update notice: could not read the database"),
        }
    }

    async fn apply(&self, d: Decision, published: &str) {
        let res = match &d {
            Decision::Keep => return,
            Decision::Remove => self.store.delete_auto_notices().await.map(|n| {
                if n > 0 {
                    info!("automatic update notice removed");
                }
            }),
            Decision::Upsert(n) => self.store.upsert_auto_notice(n).await.map(|id| {
                info!(
                    id,
                    starts_at = n.starts_at,
                    show_from = n.show_from,
                    published = %ghcr::short(published),
                    "automatic update notice scheduled"
                );
            }),
        };
        if let Err(e) = res {
            warn!(error = %e, "automatic update notice: could not write the database");
            return;
        }
        match self.store.notices().await {
            Ok(list) => self.board.set_notices(list),
            Err(e) => warn!(error = %e, "automatic update notice: could not reload the notices"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::DateTime;

    const RUNNING: &str = "fbd1347";
    const SAME: &str = "fbd1347a0b1c2d3e4f5a6b7c8d9e0f1a2b3c4d5e";
    const NEWER: &str = "0123456789abcdef0123456789abcdef01234567";

    fn utc(s: &str) -> i64 {
        DateTime::parse_from_rfc3339(s).unwrap().timestamp()
    }

    fn valid() -> Valid {
        Settings {
            enabled: true,
            timezone: "UTC".into(),
            ..Settings::default()
        }
        .validate()
        .unwrap()
    }

    fn facts<'a>(v: &'a Valid, published: &'a str, existing: Option<&'a Notice>, now: i64) -> Facts<'a> {
        Facts {
            enabled: v.settings.enabled,
            running: Some(RUNNING),
            published,
            valid: v,
            locale: "en",
            existing,
            now,
        }
    }

    fn as_notice(n: &NewNotice, id: i64) -> Notice {
        Notice {
            id,
            kind: n.kind,
            title: n.title.clone(),
            message: n.message.clone(),
            starts_at: n.starts_at,
            ends_at: n.ends_at,
            show_from: n.show_from,
            active: n.active,
            created_at: 0,
            updated_at: 0,
            auto: true,
        }
    }

    fn upsert(d: Decision) -> NewNotice {
        match d {
            Decision::Upsert(n) => n,
            other => panic!("expected an upsert, got {other:?}"),
        }
    }

    #[test]
    fn defaults_validate_and_bad_settings_are_refused() {
        let v = Settings::default().validate().unwrap();
        assert_eq!(v.image.to_string(), DEFAULT_IMAGE);
        assert!(!v.settings.enabled);
        assert_eq!(v.settings.duration_minutes, 5);
        assert_eq!(v.settings.check_interval_minutes, 60);
        let ok = Settings {
            time: "4:00".into(),
            image: " ghcr.io/pixlgalaxy/lynx:sha-fbd1347 ".into(),
            timezone: "America/Mexico_City".into(),
            ..Settings::default()
        }
        .validate()
        .unwrap();
        assert_eq!(ok.settings.time, "04:00", "normalized");
        assert_eq!(ok.settings.image, "ghcr.io/pixlgalaxy/lynx:sha-fbd1347");
        for bad in [
            Settings {
                image: "docker.io/x/y:z".into(),
                ..Settings::default()
            },
            Settings {
                time: "25:00".into(),
                ..Settings::default()
            },
            Settings {
                timezone: "Nowhere/City".into(),
                ..Settings::default()
            },
            Settings {
                lead_hours: 0,
                ..Settings::default()
            },
            Settings {
                lead_hours: MAX_LEAD_HOURS + 1,
                ..Settings::default()
            },
            Settings {
                duration_minutes: 0,
                ..Settings::default()
            },
            Settings {
                check_interval_minutes: 9,
                ..Settings::default()
            },
            Settings {
                check_interval_minutes: MAX_INTERVAL_MINUTES + 1,
                ..Settings::default()
            },
            Settings {
                templates: Templates {
                    en: Template {
                        title: " ".into(),
                        message: "".into(),
                    },
                    ..Templates::default()
                },
                ..Settings::default()
            },
            Settings {
                templates: Templates {
                    es: Template {
                        title: "x".repeat(MAX_TITLE + 1),
                        message: "".into(),
                    },
                    ..Templates::default()
                },
                ..Settings::default()
            },
        ] {
            assert!(bad.clone().validate().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn saved_settings_round_trip_and_fall_back_to_defaults() {
        let v = valid();
        let raw = serde_json::to_string(&v.settings).unwrap();
        assert!(raw.contains("\"checkIntervalMinutes\":60"), "{raw}");
        assert_eq!(Settings::from_saved(Some(&raw)).settings, v.settings);
        // Missing fields take the defaults; junk or invalid values fall back entirely.
        assert!(Settings::from_saved(Some(r#"{"enabled":true}"#)).settings.enabled);
        assert!(!Settings::from_saved(Some("not json")).settings.enabled);
        assert!(
            !Settings::from_saved(Some(r#"{"enabled":true,"time":"99:99"}"#))
                .settings
                .enabled
        );
        assert!(
            serde_json::from_str::<Settings>(r#"{"surprise":1}"#).is_err(),
            "unknown fields refused"
        );
    }

    #[test]
    fn templates_follow_the_bot_locale() {
        let t = Templates::default();
        assert_eq!(t.for_locale("es").title, "Actualización programada");
        assert_eq!(t.for_locale("es-419").title, "Actualización programada");
        assert_eq!(t.for_locale("en").title, "Scheduled update");
        assert_eq!(t.for_locale("fr").title, "Scheduled update");
        assert_eq!(
            render(&t.es, 5).1,
            "Lynx se actualizará a una versión nueva. Tomará unos 5 minutos."
        );
    }

    #[test]
    fn new_image_creates_a_notice_at_the_next_window_with_lead_time() {
        let v = valid(); // daily 04:00 UTC, 24 h lead, 5 minutes
        let now = utc("2026-11-24T10:00:00Z");
        let n = upsert(decide(&facts(&v, NEWER, None, now)));
        assert_eq!(n.kind, Kind::Maintenance);
        assert_eq!(n.starts_at, utc("2026-11-25T04:00:00Z"));
        assert_eq!(n.ends_at, n.starts_at + 5 * 60);
        assert_eq!(n.show_from, now, "less than the lead time away: shown right away");
        assert_eq!(n.title, "Scheduled update");
        assert_eq!(
            n.message,
            "Lynx will update to a new version. It takes about 5 minutes."
        );
        assert!(n.active);

        // Weekly with 3 days of lead: shown from 72 h before the window.
        let weekly = Settings {
            frequency: Frequency::Weekly,
            weekday: Day::Sun,
            lead_hours: 72,
            ..v.settings.clone()
        }
        .validate()
        .unwrap();
        let n = upsert(decide(&facts(&weekly, NEWER, None, now)));
        assert_eq!(n.starts_at, utc("2026-11-29T04:00:00Z"));
        assert_eq!(n.show_from, utc("2026-11-26T04:00:00Z"));

        // Spanish bot.
        let mut f = facts(&v, NEWER, None, now);
        f.locale = "es";
        assert_eq!(upsert(decide(&f)).title, "Actualización programada");
    }

    #[test]
    fn same_commit_or_unknown_build_or_disabled_removes() {
        let v = valid();
        let now = utc("2026-11-24T10:00:00Z");
        let n = as_notice(&upsert(decide(&facts(&v, NEWER, None, now))), 7);
        assert_eq!(
            decide(&facts(&v, SAME, Some(&n), now)),
            Decision::Remove,
            "updated: removed"
        );
        assert_eq!(decide(&facts(&v, SAME, None, now)), Decision::Keep, "nothing to remove");
        let mut f = facts(&v, NEWER, Some(&n), now);
        f.running = None;
        assert_eq!(decide(&f), Decision::Remove, "dev build: no notice");
        f.existing = None;
        assert_eq!(decide(&f), Decision::Keep, "dev build never creates one");
        let mut f = facts(&v, NEWER, Some(&n), now);
        f.enabled = false;
        assert_eq!(decide(&f), Decision::Remove);
    }

    #[test]
    fn existing_notice_is_kept_then_rolled_when_the_window_passes() {
        let v = valid();
        let now = utc("2026-11-24T10:00:00Z");
        let first = upsert(decide(&facts(&v, NEWER, None, now)));
        let n = as_notice(&first, 7);
        // Next hourly checks: nothing changes (show_from is not dragged along with now).
        assert_eq!(decide(&facts(&v, NEWER, Some(&n), now + 3600)), Decision::Keep);
        // An even newer image: the same notice stays.
        let newest = "fedcba9876543210fedcba9876543210fedcba98";
        assert_eq!(decide(&facts(&v, newest, Some(&n), now + 7200)), Decision::Keep);
        // During the update window: kept (no roll mid-update).
        assert_eq!(decide(&facts(&v, NEWER, Some(&n), n.starts_at)), Decision::Keep);
        assert_eq!(decide(&facts(&v, NEWER, Some(&n), n.ends_at - 1)), Decision::Keep);
        // The window passed and the bot still runs the old commit: roll to the next day.
        let later = n.ends_at + 60;
        let rolled = upsert(decide(&facts(&v, NEWER, Some(&n), later)));
        assert_eq!(rolled.starts_at, utc("2026-11-26T04:00:00Z"));
        assert_eq!(rolled.show_from, later, "24 h lead: visible again right away");
        // New text in the settings updates the notice in place.
        let mut s = v.settings.clone();
        s.templates.en.title = "Update coming".into();
        let v2 = s.validate().unwrap();
        assert_eq!(
            upsert(decide(&facts(&v2, NEWER, Some(&n), now + 60))).title,
            "Update coming"
        );
        // A longer duration too.
        let v3 = Settings {
            duration_minutes: 15,
            ..v.settings.clone()
        }
        .validate()
        .unwrap();
        let d = upsert(decide(&facts(&v3, NEWER, Some(&n), now + 60)));
        assert_eq!(d.ends_at - d.starts_at, 15 * 60);
        assert_eq!(d.show_from, n.show_from, "still shown since then");
    }

    #[test]
    fn show_from_respects_the_lead_time_and_now() {
        let v = Settings {
            lead_hours: 3,
            ..valid().settings
        }
        .validate()
        .unwrap();
        // 10:00 -> window tomorrow 04:00; 3 h lead: shown from 01:00 tomorrow.
        let now = utc("2026-11-24T10:00:00Z");
        let n = upsert(decide(&facts(&v, NEWER, None, now)));
        assert_eq!(n.show_from, utc("2026-11-25T01:00:00Z"));
        // Not visible yet, lead time grows: show_from follows.
        let existing = as_notice(&n, 3);
        let v24 = Settings {
            lead_hours: 24,
            ..v.settings.clone()
        }
        .validate()
        .unwrap();
        assert_eq!(upsert(decide(&facts(&v24, NEWER, Some(&existing), now))).show_from, now);
        // Exactly at the window: the next window is taken, not this one.
        let at = utc("2026-11-25T04:00:00Z");
        assert_eq!(
            upsert(decide(&facts(&v, NEWER, None, at))).starts_at,
            utc("2026-11-26T04:00:00Z")
        );
    }
}
