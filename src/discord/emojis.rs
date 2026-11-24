//! Button icons, uploaded by the main bot as Discord application emojis.
//!
//! The PNGs live in `assets/emojis/` (made from lucide by `scripts/gen-emojis.mjs`) and are embedded
//! in the binary. After the main bot is ready, [`sync_main`] lists the application's emojis, uploads
//! the missing ones and keeps a name -> id map in memory. Buttons only carry an emoji once its id is
//! known, so before the sync finishes (or when it fails) they render text-only.
//!
//! On grey buttons the icon is colored; on colored buttons it is white. Link buttons never carry
//! an icon: Discord already draws its own external-link arrow on them.

use std::{
    collections::HashMap,
    sync::{OnceLock, RwLock},
    time::Duration,
};

use serde_json::json;
use serenity::{async_trait, http::Http};
use tracing::{info, warn};

use super::ui::ButtonStyle;

const TIMEOUT: Duration = Duration::from_secs(15);
/// Pause between uploads; serenity also follows Discord's rate limit headers.
const UPLOAD_GAP: Duration = Duration::from_millis(250);

macro_rules! emoji {
    ($name:literal) => {
        (
            $name,
            include_bytes!(concat!("../../assets/emojis/", $name, ".png")) as &[u8],
        )
    };
}

/// Every emoji the bot uploads (name, PNG). Keep in sync with `scripts/gen-emojis.mjs`.
pub const EMOJIS: &[(&str, &[u8])] = &[
    emoji!("lynx_pause_sky"),
    emoji!("lynx_play_sky"),
    emoji!("lynx_stop_red"),
    emoji!("lynx_skip_emerald"),
    emoji!("lynx_queue_amber"),
    emoji!("lynx_jam_violet"),
    emoji!("lynx_prev_slate"),
    emoji!("lynx_next_slate"),
    emoji!("lynx_replay_sky"),
    emoji!("lynx_radio_red"),
    emoji!("lynx_radio_white"),
    emoji!("lynx_like_white"),
    emoji!("lynx_dislike_white"),
    emoji!("lynx_panel_white"),
    emoji!("lynx_cancel_white"),
    emoji!("lynx_select_white"),
];

/// The lucide icon of a button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    Pause,
    Play,
    Stop,
    Skip,
    Queue,
    Like,
    Dislike,
    Jam,
    Panel,
    Previous,
    Next,
    Replay,
    Radio,
    Cancel,
    /// A check mark (Select on the playlist picker).
    Select,
}

impl Icon {
    fn key(self) -> &'static str {
        match self {
            Self::Pause => "pause",
            Self::Play => "play",
            Self::Stop => "stop",
            Self::Skip => "skip",
            Self::Queue => "queue",
            Self::Like => "like",
            Self::Dislike => "dislike",
            Self::Jam => "jam",
            Self::Panel => "panel",
            Self::Previous => "prev",
            Self::Next => "next",
            Self::Replay => "replay",
            Self::Radio => "radio",
            Self::Cancel => "cancel",
            Self::Select => "select",
        }
    }

    /// Color of the icon on a grey button.
    fn tint(self) -> &'static str {
        match self {
            Self::Pause | Self::Play | Self::Replay => "sky",
            Self::Stop | Self::Radio => "red",
            Self::Skip => "emerald",
            Self::Queue => "amber",
            Self::Jam => "violet",
            Self::Previous | Self::Next => "slate",
            Self::Like | Self::Dislike | Self::Panel | Self::Cancel => "white",
            // Only shipped for colored buttons.
            Self::Select => "emerald",
        }
    }

    /// Emoji name for this icon on a button of `style`; None when no such variant is shipped.
    pub fn emoji_name(self, style: ButtonStyle) -> Option<&'static str> {
        let color = match style {
            ButtonStyle::Secondary => self.tint(),
            ButtonStyle::Primary | ButtonStyle::Success | ButtonStyle::Danger => "white",
            ButtonStyle::Link => return None,
        };
        let name = format!("lynx_{}_{color}", self.key());
        EMOJIS.iter().map(|(n, _)| *n).find(|n| *n == name)
    }
}

fn ids() -> &'static RwLock<HashMap<String, u64>> {
    static IDS: OnceLock<RwLock<HashMap<String, u64>>> = OnceLock::new();
    IDS.get_or_init(Default::default)
}

/// Id of an uploaded emoji, once the sync found or created it.
pub fn emoji_id(name: &str) -> Option<u64> {
    ids().read().ok()?.get(name).copied()
}

fn install(map: HashMap<String, u64>) {
    if let Ok(mut ids) = ids().write() {
        ids.extend(map);
    }
}

/// The application emoji endpoints (a fake in tests).
#[async_trait]
pub trait EmojiApi: Sync {
    /// `GET /applications/{id}/emojis` as (name, id).
    async fn list(&self) -> Result<Vec<(String, u64)>, String>;
    /// `POST /applications/{id}/emojis`; `image` is a `data:` URI.
    async fn create(&self, name: &str, image: &str) -> Result<u64, String>;
}

struct SerenityApi<'a>(&'a Http);

#[async_trait]
impl EmojiApi for SerenityApi<'_> {
    async fn list(&self) -> Result<Vec<(String, u64)>, String> {
        match tokio::time::timeout(TIMEOUT, self.0.get_application_emojis()).await {
            Ok(Ok(list)) => Ok(list.into_iter().map(|e| (e.name, e.id.get())).collect()),
            Ok(Err(e)) => Err(e.to_string()),
            Err(_) => Err("timed out".into()),
        }
    }

    async fn create(&self, name: &str, image: &str) -> Result<u64, String> {
        let body = json!({ "name": name, "image": image });
        match tokio::time::timeout(TIMEOUT, self.0.create_application_emoji(&body)).await {
            Ok(Ok(e)) => Ok(e.id.get()),
            Ok(Err(e)) => Err(e.to_string()),
            Err(_) => Err("timed out".into()),
        }
    }
}

/// Find or upload every emoji in `wanted`; returns name -> id for the ones that exist now.
/// Never deletes anything. A failed upload (for example a name taken by a concurrent upload) is
/// followed by one more listing to pick up ids created meanwhile.
pub async fn sync(api: &impl EmojiApi, wanted: &[(&str, &[u8])], gap: Duration) -> HashMap<String, u64> {
    let existing: HashMap<String, u64> = match api.list().await {
        Ok(list) => list.into_iter().collect(),
        Err(e) => {
            warn!(error = %e, "could not list the application emojis; buttons stay text-only");
            return HashMap::new();
        }
    };
    let mut found: HashMap<String, u64> = wanted
        .iter()
        .filter_map(|(name, _)| existing.get(*name).map(|id| ((*name).to_owned(), *id)))
        .collect();
    let mut failed = false;
    let mut uploaded = 0;
    let missing: Vec<_> = wanted.iter().filter(|(n, _)| !found.contains_key(*n)).collect();
    for (name, png) in missing {
        if uploaded > 0 && !gap.is_zero() {
            tokio::time::sleep(gap).await;
        }
        let image = format!("data:image/png;base64,{}", base64(png));
        match api.create(name, &image).await {
            Ok(id) => {
                uploaded += 1;
                found.insert((*name).to_owned(), id);
            }
            Err(e) => {
                failed = true;
                warn!(emoji = name, error = %e, "could not upload an application emoji");
            }
        }
    }
    if failed {
        if let Ok(list) = api.list().await {
            for (name, id) in list {
                if wanted.iter().any(|(n, _)| *n == name) {
                    found.entry(name).or_insert(id);
                }
            }
        }
    }
    info!(
        ready = found.len(),
        uploaded,
        total = wanted.len(),
        "application emojis synced"
    );
    found
}

/// Sync the button emojis with the main bot's HTTP client and make them available to the cards.
pub async fn sync_main(http: &Http) {
    let map = sync(&SerenityApi(http), EMOJIS, UPLOAD_GAP).await;
    install(map);
}

/// Standard base64 with padding.
fn base64(data: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ABC[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct Fake {
        emojis: Mutex<Vec<(String, u64)>>,
        lists: Mutex<usize>,
        creates: Mutex<Vec<String>>,
        /// Names whose upload fails but which appear on the next listing (a concurrent upload).
        taken: Vec<&'static str>,
        list_fails: bool,
    }

    #[async_trait]
    impl EmojiApi for Fake {
        async fn list(&self) -> Result<Vec<(String, u64)>, String> {
            *self.lists.lock().unwrap() += 1;
            if self.list_fails {
                return Err("boom".into());
            }
            Ok(self.emojis.lock().unwrap().clone())
        }

        async fn create(&self, name: &str, image: &str) -> Result<u64, String> {
            assert!(image.starts_with("data:image/png;base64,iVBORw0KGgo"), "{image:.40}");
            self.creates.lock().unwrap().push(name.to_owned());
            let mut emojis = self.emojis.lock().unwrap();
            let id = 1000 + emojis.len() as u64;
            emojis.push((name.to_owned(), id));
            if self.taken.contains(&name) {
                return Err("name already taken".into());
            }
            Ok(id)
        }
    }

    #[tokio::test]
    async fn sync_uploads_only_missing_and_is_idempotent() {
        let fake = Fake::default();
        fake.emojis
            .lock()
            .unwrap()
            .extend([("lynx_pause_sky".to_owned(), 7), ("someone_elses".to_owned(), 8)]);
        let map = sync(&fake, EMOJIS, Duration::ZERO).await;
        assert_eq!(map.len(), EMOJIS.len());
        assert_eq!(map["lynx_pause_sky"], 7, "existing ids are reused");
        assert!(!map.contains_key("someone_elses"), "unknown emojis are ignored");
        let created = fake.creates.lock().unwrap().clone();
        assert_eq!(created.len(), EMOJIS.len() - 1);
        assert!(!created.contains(&"lynx_pause_sky".to_owned()));
        assert_eq!(*fake.lists.lock().unwrap(), 1);

        // Second run: everything exists, nothing is uploaded, same ids.
        let again = sync(&fake, EMOJIS, Duration::ZERO).await;
        assert_eq!(again, map);
        assert_eq!(fake.creates.lock().unwrap().len(), EMOJIS.len() - 1);
        assert_eq!(fake.emojis.lock().unwrap().len(), EMOJIS.len() + 1, "nothing deleted");
    }

    #[tokio::test]
    async fn sync_relists_after_a_failed_upload() {
        let fake = Fake {
            taken: vec!["lynx_stop_red"],
            ..Default::default()
        };
        let map = sync(&fake, &EMOJIS[..3], Duration::ZERO).await;
        assert_eq!(map.len(), 3, "{map:?}");
        assert!(map.contains_key("lynx_stop_red"));
        assert_eq!(*fake.lists.lock().unwrap(), 2);
    }

    #[tokio::test]
    async fn sync_gives_up_when_listing_fails() {
        let fake = Fake {
            list_fails: true,
            ..Default::default()
        };
        assert!(sync(&fake, EMOJIS, Duration::ZERO).await.is_empty());
        assert!(fake.creates.lock().unwrap().is_empty());
    }

    #[test]
    fn emoji_variants_per_button_style() {
        use ButtonStyle::*;
        let cases = [
            (Icon::Pause, Secondary, "lynx_pause_sky"),
            (Icon::Play, Secondary, "lynx_play_sky"),
            (Icon::Stop, Secondary, "lynx_stop_red"),
            (Icon::Skip, Secondary, "lynx_skip_emerald"),
            (Icon::Queue, Secondary, "lynx_queue_amber"),
            (Icon::Jam, Secondary, "lynx_jam_violet"),
            (Icon::Previous, Secondary, "lynx_prev_slate"),
            (Icon::Next, Secondary, "lynx_next_slate"),
            (Icon::Replay, Secondary, "lynx_replay_sky"),
            (Icon::Radio, Secondary, "lynx_radio_red"),
            (Icon::Radio, Primary, "lynx_radio_white"),
            (Icon::Like, Success, "lynx_like_white"),
            (Icon::Dislike, Danger, "lynx_dislike_white"),
            (Icon::Panel, Primary, "lynx_panel_white"),
            (Icon::Cancel, Danger, "lynx_cancel_white"),
            (Icon::Select, Primary, "lynx_select_white"),
        ];
        for (icon, style, name) in cases {
            assert_eq!(icon.emoji_name(style), Some(name), "{icon:?} on {style:?}");
        }
        // A variant that is not shipped gives no emoji.
        assert_eq!(Icon::Pause.emoji_name(Primary), None);
        // Every shipped emoji is reachable, valid and small enough for Discord.
        for (name, png) in EMOJIS {
            assert!(cases.iter().any(|(_, _, n)| n == name), "{name} is unused");
            assert!((2..=32).contains(&name.len()));
            assert!(name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
            assert!(png.starts_with(b"\x89PNG") && png.len() <= 256 * 1024, "{name}");
        }
    }

    /// Live: `DISCORD_TOKEN=... cargo test live_sync -- --ignored` uploads to the real application.
    #[tokio::test]
    #[ignore]
    async fn live_sync() {
        let Some(token) = std::env::var("DISCORD_TOKEN").ok().filter(|t| !t.is_empty()) else {
            eprintln!("DISCORD_TOKEN not set; skipping");
            return;
        };
        let http = Http::new(&token);
        let app = http.get_current_application_info().await.expect("application info");
        http.set_application_id(app.id);
        let first = sync(&SerenityApi(&http), EMOJIS, UPLOAD_GAP).await;
        assert_eq!(first.len(), EMOJIS.len(), "{first:?}");
        let again = sync(&SerenityApi(&http), EMOJIS, UPLOAD_GAP).await;
        assert_eq!(again, first, "second run reuses the same emojis");
        let listed = SerenityApi(&http).list().await.expect("list");
        for (name, _) in EMOJIS {
            let id = listed.iter().find(|(n, _)| n == name).map(|(_, id)| *id);
            assert_eq!(id, first.get(*name).copied(), "{name}");
            eprintln!("{name} = {}", id.unwrap_or(0));
        }
    }

    #[test]
    fn base64_matches_rfc4648() {
        for (input, out) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(input.as_bytes()), out);
        }
    }
}
