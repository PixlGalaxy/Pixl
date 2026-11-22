//! The bot's message designs. No emojis; every string comes from i18n (`bot.*` keys).

use super::{
    escape, format_duration, link, relative_time, truncate, Button, ButtonStyle, Component, Message, COLOR_ERROR,
    COLOR_INFO, COLOR_SUCCESS, COLOR_WARNING,
};
use crate::{
    discord::ids::{CustomId, NpAction},
    i18n::I18n,
    store::LoopMode,
};

/// Queue entries per page.
pub const QUEUE_PAGE_SIZE: usize = 10;

/// What a card needs to know about one track.
#[derive(Debug, Clone)]
pub struct TrackCard<'a> {
    pub title: &'a str,
    pub url: &'a str,
    pub artists: &'a [String],
    pub duration_ms: Option<u64>,
    pub thumbnail: Option<&'a str>,
    pub requester_id: u64,
}

impl TrackCard<'_> {
    fn duration(&self, i18n: &I18n) -> String {
        match self.duration_ms {
            Some(ms) if ms > 0 => format_duration(ms),
            _ => i18n.t("bot.track.live", &[]),
        }
    }

    fn artists(&self) -> String {
        escape(&truncate(&self.artists.join(", "), 120))
    }

    /// `Artist · 3:07 · Requested by @user`
    fn meta_line(&self, i18n: &I18n) -> String {
        let mut parts = Vec::with_capacity(3);
        if !self.artists.is_empty() {
            parts.push(self.artists());
        }
        parts.push(self.duration(i18n));
        parts.push(i18n.t(
            "bot.track.requested_by",
            &[("user", &format!("<@{}>", self.requester_id))],
        ));
        parts.join(" · ")
    }
}

#[derive(Debug, Clone)]
pub struct NowPlaying<'a> {
    pub track: TrackCard<'a>,
    pub voice_channel_id: u64,
    /// Name the voice channel (the track plays on a node bot, or the guild has several players).
    pub show_channel: bool,
    pub genres: &'a [String],
    pub paused: bool,
    pub ai_enabled: bool,
    /// `ids::track_tag` of the entry.
    pub track_tag: &'a str,
    pub upcoming: usize,
    pub loop_mode: LoopMode,
}

pub fn now_playing(v: &NowPlaying, i18n: &I18n) -> Message {
    let header = if v.show_channel {
        i18n.t(
            "bot.now_playing.title_in",
            &[("channel", &format!("<#{}>", v.voice_channel_id))],
        )
    } else {
        i18n.t("bot.now_playing.title", &[])
    };
    let mut texts = vec![
        format!("-# {header}"),
        format!("### {}", link(v.track.title, v.track.url)),
        v.track.meta_line(i18n),
    ];
    if !v.genres.is_empty() {
        let genres = v
            .genres
            .iter()
            .take(4)
            .map(|g| escape(g))
            .collect::<Vec<_>>()
            .join(", ");
        texts.push(format!(
            "-# {}",
            i18n.t("bot.now_playing.genres", &[("genres", &genres)])
        ));
    }
    let mut children = Component::section_or_text(texts, v.track.thumbnail);

    let mut status = Vec::new();
    if v.paused {
        status.push(i18n.t("bot.now_playing.paused", &[]));
    }
    status.push(if v.upcoming == 0 {
        i18n.t("bot.now_playing.up_next_none", &[])
    } else {
        i18n.t("bot.now_playing.up_next", &[("count", &v.upcoming.to_string())])
    });
    if v.loop_mode != LoopMode::Off {
        status.push(loop_label(v.loop_mode, i18n));
    }
    children.push(Component::separator());
    children.push(Component::text(format!("-# {}", status.join(" · "))));

    let id = |action| {
        CustomId::NowPlaying {
            action,
            voice_channel_id: v.voice_channel_id,
            track: v.track_tag.to_owned(),
        }
        .encode()
    };
    let toggle = if v.paused {
        Button::new(
            ButtonStyle::Primary,
            i18n.t("bot.button.resume", &[]),
            id(NpAction::Resume),
        )
    } else {
        Button::new(
            ButtonStyle::Primary,
            i18n.t("bot.button.pause", &[]),
            id(NpAction::Pause),
        )
    };
    children.push(Component::Row(vec![
        toggle,
        Button::new(
            ButtonStyle::Secondary,
            i18n.t("bot.button.skip", &[]),
            id(NpAction::Skip),
        ),
        Button::new(ButtonStyle::Danger, i18n.t("bot.button.stop", &[]), id(NpAction::Stop)),
        Button::new(
            ButtonStyle::Secondary,
            i18n.t("bot.button.queue", &[]),
            id(NpAction::Queue),
        ),
        Button::new(
            ButtonStyle::Secondary,
            i18n.t("bot.button.panel", &[]),
            id(NpAction::Panel),
        ),
    ]));
    if v.ai_enabled {
        children.push(Component::Row(vec![
            Button::new(ButtonStyle::Success, i18n.t("bot.button.like", &[]), id(NpAction::Like)),
            Button::new(
                ButtonStyle::Secondary,
                i18n.t("bot.button.dislike", &[]),
                id(NpAction::Dislike),
            ),
        ]));
    }
    Message::new(vec![Component::Container {
        accent: Some(COLOR_INFO),
        children,
    }])
}

pub fn loop_label(mode: LoopMode, i18n: &I18n) -> String {
    match mode {
        LoopMode::Off => i18n.t("bot.loop.off", &[]),
        LoopMode::Track => i18n.t("bot.loop.track", &[]),
        LoopMode::Queue => i18n.t("bot.loop.queue", &[]),
    }
}

#[derive(Debug, Clone)]
pub struct QueuePage<'a> {
    pub voice_channel_id: u64,
    pub current: Option<TrackCard<'a>>,
    pub paused: bool,
    /// The tracks of this page only, with their 1-based positions in the upcoming list.
    pub entries: Vec<(usize, TrackCard<'a>)>,
    /// 0-based.
    pub page: u32,
    pub total_upcoming: usize,
    pub total_duration_ms: u64,
    pub loop_mode: LoopMode,
    /// Unix seconds; the buttons expire after `ids::QUEUE_BUTTONS_TTL_SECS`.
    pub issued: i64,
}

pub fn page_count(total: usize) -> u32 {
    total.div_ceil(QUEUE_PAGE_SIZE).max(1) as u32
}

pub fn queue_page(v: &QueuePage, i18n: &I18n) -> Message {
    let pages = page_count(v.total_upcoming);
    let mut children = vec![Component::text(format!(
        "### {}\n-# {}",
        i18n.t("bot.queue.title", &[]),
        i18n.t(
            "bot.queue.in_channel",
            &[("channel", &format!("<#{}>", v.voice_channel_id))]
        )
    ))];
    if let Some(cur) = &v.current {
        let key = if v.paused {
            "bot.queue.current_paused"
        } else {
            "bot.queue.current"
        };
        children.push(Component::text(format!(
            "{}\n{} · {}",
            i18n.t(key, &[]),
            link(&truncate(cur.title, 80), cur.url),
            cur.duration(i18n)
        )));
    }
    children.push(Component::separator());
    if v.entries.is_empty() {
        children.push(Component::text(i18n.t("bot.queue.empty", &[])));
    } else {
        let lines: Vec<String> = v
            .entries
            .iter()
            .map(|(pos, t)| {
                format!(
                    "`{pos}.` {} · {} · <@{}>",
                    link(&truncate(t.title, 60), t.url),
                    t.duration(i18n),
                    t.requester_id
                )
            })
            .collect();
        children.push(Component::text(lines.join("\n")));
    }
    let mut footer = vec![
        i18n.t(
            "bot.queue.page",
            &[("page", &(v.page + 1).to_string()), ("pages", &pages.to_string())],
        ),
        i18n.t("bot.queue.count", &[("count", &v.total_upcoming.to_string())]),
    ];
    if v.total_duration_ms > 0 {
        footer.push(format_duration(v.total_duration_ms));
    }
    if v.loop_mode != LoopMode::Off {
        footer.push(loop_label(v.loop_mode, i18n));
    }
    children.push(Component::text(format!("-# {}", footer.join(" · "))));
    if pages > 1 {
        let id = |page| {
            CustomId::QueuePage {
                voice_channel_id: v.voice_channel_id,
                page,
                issued: v.issued,
            }
            .encode()
        };
        children.push(Component::Row(vec![
            Button::new(
                ButtonStyle::Secondary,
                i18n.t("bot.button.previous", &[]),
                id(v.page.saturating_sub(1)),
            )
            .disabled(v.page == 0),
            Button::new(ButtonStyle::Secondary, i18n.t("bot.button.next", &[]), id(v.page + 1))
                .disabled(v.page + 1 >= pages),
        ]));
    }
    Message::new(vec![Component::Container {
        accent: Some(COLOR_INFO),
        children,
    }])
}

#[derive(Debug, Clone)]
pub enum Added<'a> {
    /// `position` is 0 when it starts playing right away.
    Single { track: TrackCard<'a>, position: usize },
    Playlist {
        name: Option<&'a str>,
        count: usize,
        first: TrackCard<'a>,
        truncated: bool,
    },
}

pub fn added(v: &Added, i18n: &I18n) -> Message {
    let children = match v {
        Added::Single { track, position } => {
            let pos = if *position == 0 {
                i18n.t("bot.added.playing_now", &[])
            } else {
                i18n.t("bot.added.position", &[("position", &position.to_string())])
            };
            Component::section_or_text(
                vec![
                    format!("-# {}", i18n.t("bot.added.title", &[])),
                    format!("**{}**", link(track.title, track.url)),
                    format!("{}\n-# {pos}", track.meta_line(i18n)),
                ],
                track.thumbnail,
            )
        }
        Added::Playlist {
            name,
            count,
            first,
            truncated,
        } => {
            let headline = match name {
                Some(n) => i18n.t(
                    "bot.added.playlist_named",
                    &[("count", &count.to_string()), ("name", &escape(&truncate(n, 100)))],
                ),
                None => i18n.t("bot.added.playlist", &[("count", &count.to_string())]),
            };
            let mut texts = vec![
                format!("-# {}", i18n.t("bot.added.title", &[])),
                format!("**{headline}**"),
                i18n.t(
                    "bot.added.first",
                    &[("track", &link(&truncate(first.title, 80), first.url))],
                ),
            ];
            if *truncated {
                texts.push(format!("-# {}", i18n.t("bot.added.truncated", &[])));
            }
            Component::section_or_text(texts, first.thumbnail)
        }
    };
    Message::new(vec![Component::Container {
        accent: Some(COLOR_SUCCESS),
        children,
    }])
}

/// Automatic messages in the text channel. One status message per player replaces the previous.
#[derive(Debug, Clone)]
pub enum Notice<'a> {
    PausedEmpty { deadline_secs: i64 },
    ResumedJoin,
    QueueDeleted,
    ResumedAfterRestart { title: &'a str, url: &'a str },
    QueueEnded,
    TrackFailed { title: &'a str, reason: &'a str },
    TooManyFailures,
    Disconnected,
    Stopped { by: u64 },
}

pub fn notice(n: &Notice, voice_channel_id: u64, i18n: &I18n) -> Message {
    let channel = format!("<#{voice_channel_id}>");
    let (color, text) = match n {
        Notice::PausedEmpty { deadline_secs } => (
            COLOR_WARNING,
            i18n.t(
                "bot.notice.paused_empty",
                &[("channel", &channel), ("time", &relative_time(*deadline_secs))],
            ),
        ),
        Notice::ResumedJoin => (
            COLOR_SUCCESS,
            i18n.t("bot.notice.resumed_join", &[("channel", &channel)]),
        ),
        Notice::QueueDeleted => (
            COLOR_WARNING,
            i18n.t("bot.notice.queue_deleted", &[("channel", &channel)]),
        ),
        Notice::ResumedAfterRestart { title, url } => (
            COLOR_INFO,
            i18n.t(
                "bot.notice.resumed_restart",
                &[("channel", &channel), ("track", &link(&truncate(title, 80), url))],
            ),
        ),
        Notice::QueueEnded => (COLOR_INFO, i18n.t("bot.notice.queue_ended", &[("channel", &channel)])),
        Notice::TrackFailed { title, reason } => (
            COLOR_ERROR,
            i18n.t(
                "bot.notice.track_failed",
                &[
                    ("track", &escape(&truncate(title, 80))),
                    ("reason", &escape(&truncate(reason, 200))),
                ],
            ),
        ),
        Notice::TooManyFailures => (
            COLOR_ERROR,
            i18n.t("bot.notice.too_many_failures", &[("channel", &channel)]),
        ),
        Notice::Disconnected => (
            COLOR_WARNING,
            i18n.t("bot.notice.disconnected", &[("channel", &channel)]),
        ),
        Notice::Stopped { by } => (
            COLOR_INFO,
            i18n.t(
                "bot.notice.stopped",
                &[("channel", &channel), ("user", &format!("<@{by}>"))],
            ),
        ),
    };
    Message::new(vec![Component::Container {
        accent: Some(color),
        children: vec![Component::text(text)],
    }])
}

/// One-line reply (ephemeral by default) for command results.
pub fn reply(text: impl Into<String>, color: u32) -> Message {
    Message::new(vec![Component::Container {
        accent: Some(color),
        children: vec![Component::text(text)],
    }])
    .ephemeral(true)
}

pub fn error(text: impl Into<String>) -> Message {
    reply(text, COLOR_ERROR)
}

pub fn success(text: impl Into<String>) -> Message {
    reply(text, COLOR_SUCCESS)
}

/// Error with a link button (invite the next node / the main bot).
pub fn error_with_link(text: impl Into<String>, label: impl Into<String>, url: &str) -> Message {
    Message::new(vec![Component::Container {
        accent: Some(COLOR_ERROR),
        children: vec![Component::text(text), Component::Row(vec![Button::link(label, url)])],
    }])
    .ephemeral(true)
}

pub fn lyrics(title: &str, artists: &[String], body: &str, i18n: &I18n) -> Message {
    let head = format!(
        "### {}\n-# {} - {}",
        i18n.t("bot.lyrics.title", &[]),
        escape(&truncate(title, 100)),
        escape(&truncate(&artists.join(", "), 100))
    );
    let mut text = truncate(body.trim(), 3400);
    if text.chars().count() < body.trim().chars().count() {
        text.push_str(&format!("\n\n-# {}", i18n.t("bot.lyrics.truncated", &[])));
    }
    Message::new(vec![Component::Container {
        accent: Some(COLOR_INFO),
        children: vec![Component::text(head), Component::separator(), Component::text(text)],
    }])
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;
    use crate::discord::ui::{FLAG_COMPONENTS_V2, FLAG_EPHEMERAL, MAX_TEXT_TOTAL};

    fn i18n() -> I18n {
        I18n::load("en").unwrap()
    }

    fn artists() -> Vec<String> {
        vec!["Artist".into()]
    }

    fn track<'a>(artists: &'a [String]) -> TrackCard<'a> {
        TrackCard {
            title: "Song",
            url: "https://youtu.be/x",
            artists,
            duration_ms: Some(187_000),
            thumbnail: Some("https://i.ytimg.com/x.jpg"),
            requester_id: 42,
        }
    }

    fn types(v: &Value) -> Vec<u64> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|c| c["type"].as_u64().unwrap())
            .collect()
    }

    fn custom_ids(v: &Value) -> Vec<String> {
        let mut out = Vec::new();
        fn walk(v: &Value, out: &mut Vec<String>) {
            match v {
                Value::Object(m) => {
                    if let Some(Value::String(id)) = m.get("custom_id") {
                        out.push(id.clone());
                    }
                    m.values().for_each(|x| walk(x, out));
                }
                Value::Array(a) => a.iter().for_each(|x| walk(x, out)),
                _ => {}
            }
        }
        walk(v, &mut out);
        out
    }

    fn all_text(v: &Value) -> String {
        let mut out = String::new();
        fn walk(v: &Value, out: &mut String) {
            match v {
                Value::Object(m) => {
                    for (k, x) in m {
                        if k == "content" || k == "label" {
                            out.push_str(x.as_str().unwrap_or_default());
                            out.push('\n');
                        } else {
                            walk(x, out);
                        }
                    }
                }
                Value::Array(a) => a.iter().for_each(|x| walk(x, out)),
                _ => {}
            }
        }
        walk(v, &mut out);
        out
    }

    fn assert_no_emoji(s: &str) {
        assert!(
            s.chars().all(|c| (c as u32) < 0x2190 || c == '·'),
            "emoji-like char in {s:?}"
        );
    }

    #[test]
    fn now_playing_shape() {
        let a = artists();
        let genres = vec!["pop".to_owned()];
        let mut v = NowPlaying {
            track: track(&a),
            voice_channel_id: 77,
            show_channel: true,
            genres: &genres,
            paused: false,
            ai_enabled: true,
            track_tag: "tag",
            upcoming: 3,
            loop_mode: LoopMode::Queue,
        };
        let json = now_playing(&v, &i18n()).to_json();
        assert_eq!(json["flags"], FLAG_COMPONENTS_V2);
        let container = &json["components"][0];
        assert_eq!(container["type"], 17);
        assert_eq!(container["accent_color"], COLOR_INFO);
        // section (with thumbnail), separator, status text, controls row, feedback row
        assert_eq!(types(&container["components"]), vec![9, 14, 10, 1, 1]);
        assert_eq!(container["components"][0]["accessory"]["type"], 11);
        let section = all_text(&container["components"][0]);
        assert!(section.contains("<#77>"), "{section}");
        assert!(section.contains("[Song](<https://youtu.be/x>)"));
        assert!(section.contains("3:07"));
        assert!(section.contains("<@42>"));
        assert!(section.contains("pop"));
        assert_eq!(
            custom_ids(&json),
            vec![
                "np:pause:77:tag",
                "np:skip:77:tag",
                "np:stop:77:tag",
                "np:queue:77:tag",
                "np:panel:77:tag",
                "np:like:77:tag",
                "np:dislike:77:tag"
            ]
        );
        assert_no_emoji(&all_text(&json));

        v.ai_enabled = false;
        v.paused = true;
        v.show_channel = false;
        v.genres = &[];
        let json = now_playing(&v, &i18n()).to_json();
        let container = &json["components"][0];
        assert_eq!(types(&container["components"]), vec![9, 14, 10, 1]);
        assert_eq!(custom_ids(&json)[0], "np:resume:77:tag");
        assert!(!all_text(&json).contains("<#77>"));
    }

    #[test]
    fn queue_page_shape() {
        let a = artists();
        let entries: Vec<(usize, TrackCard)> = (11..=20).map(|p| (p, track(&a))).collect();
        let v = QueuePage {
            voice_channel_id: 9,
            current: Some(track(&a)),
            paused: false,
            entries,
            page: 1,
            total_upcoming: 25,
            total_duration_ms: 25 * 187_000,
            loop_mode: LoopMode::Off,
            issued: 1000,
        };
        let json = queue_page(&v, &i18n()).to_json();
        let c = &json["components"][0]["components"];
        assert_eq!(types(c), vec![10, 10, 14, 10, 10, 1]);
        assert!(c[3]["content"].as_str().unwrap().starts_with("`11.` [Song]"));
        assert!(c[4]["content"].as_str().unwrap().contains("2 / 3") || c[4]["content"].as_str().unwrap().contains("2"));
        assert_eq!(custom_ids(&json), vec!["q:9:0:1000", "q:9:2:1000"]);
        assert!(json.to_string().len() < 8000);
        assert!(queue_page(&v, &i18n()).text_len() <= MAX_TEXT_TOTAL);

        // Last page: Next disabled; single page: no buttons.
        let last = QueuePage {
            page: 2,
            entries: vec![],
            ..v.clone()
        };
        let json = queue_page(&last, &i18n()).to_json();
        assert_eq!(
            json["components"][0]["components"][5]["components"][1]["disabled"],
            true
        );
        let single = QueuePage {
            page: 0,
            total_upcoming: 3,
            current: None,
            ..v
        };
        let json = queue_page(&single, &i18n()).to_json();
        assert!(custom_ids(&json).is_empty());
        assert_eq!(page_count(0), 1);
        assert_eq!(page_count(10), 1);
        assert_eq!(page_count(11), 2);
    }

    #[test]
    fn added_shapes() {
        let a = artists();
        let json = added(
            &Added::Single {
                track: track(&a),
                position: 4,
            },
            &i18n(),
        )
        .to_json();
        assert_eq!(json["components"][0]["accent_color"], COLOR_SUCCESS);
        assert_eq!(types(&json["components"][0]["components"]), vec![9]);
        assert!(all_text(&json).contains('4'));
        let json = added(
            &Added::Playlist {
                name: Some("Mix"),
                count: 120,
                first: track(&a),
                truncated: true,
            },
            &i18n(),
        )
        .to_json();
        let text = all_text(&json);
        assert!(text.contains("120") && text.contains("Mix"), "{text}");
        assert_no_emoji(&text);
    }

    #[test]
    fn notices_and_errors() {
        let i = i18n();
        let json = notice(
            &Notice::PausedEmpty {
                deadline_secs: 1_700_000_180,
            },
            5,
            &i,
        )
        .to_json();
        assert!(all_text(&json).contains("<t:1700000180:R>"));
        assert_eq!(json["components"][0]["accent_color"], COLOR_WARNING);
        for n in [
            Notice::ResumedJoin,
            Notice::QueueDeleted,
            Notice::ResumedAfterRestart {
                title: "T",
                url: "https://x",
            },
            Notice::QueueEnded,
            Notice::TrackFailed {
                title: "T",
                reason: "gone",
            },
            Notice::TooManyFailures,
            Notice::Disconnected,
            Notice::Stopped { by: 1 },
        ] {
            let text = all_text(&notice(&n, 5, &i).to_json());
            assert!(!text.contains("bot."), "missing translation: {text}");
            assert_no_emoji(&text);
        }
        let e = error("nope").to_json();
        assert_eq!(e["flags"], FLAG_COMPONENTS_V2 | FLAG_EPHEMERAL);
        let e = error_with_link("busy", "Invite", "https://discord.com/x").to_json();
        assert_eq!(e["components"][0]["components"][1]["components"][0]["style"], 5);
        let l = lyrics("T", &artists(), &"line\n".repeat(2000), &i).to_json();
        assert!(lyrics("T", &artists(), &"line\n".repeat(2000), &i).text_len() <= MAX_TEXT_TOTAL);
        assert_eq!(types(&l["components"][0]["components"]), vec![10, 14, 10]);
    }
}
