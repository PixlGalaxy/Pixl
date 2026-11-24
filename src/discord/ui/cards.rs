//! The bot's message designs (plan/discord-messages.html). No emojis; every string comes from i18n
//! (`bot.*` keys). Accent colors: info (blue), warn (amber), danger (red), ok (green).

use super::{
    escape, escape_line, format_duration, link, relative_time, truncate, Accessory, Button, ButtonStyle, Component, Icon, Message,
    COLOR_ERROR, COLOR_INFO, COLOR_SUCCESS, COLOR_WARNING,
};
use crate::{
    text::sanitize_text,
    ai::AiPhase,
    discord::{
        about::{YtdlpLine, DEVELOPER_URL, HOST_URL, SOURCE_URL},
        ids::{CustomId, NpAction, NO_TRACK},
    },
    i18n::I18n,
    sources::TrackOrigin,
    store::LoopMode,
};

/// Queue entries per page.
pub const QUEUE_PAGE_SIZE: usize = 10;
/// Tracks listed under "Added a playlist".
pub const PLAYLIST_PREVIEW: usize = 5;
const BAR_CELLS: usize = 26;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Info,
    Warn,
    Danger,
    Ok,
}

impl Tone {
    fn color(self) -> u32 {
        match self {
            Self::Info => COLOR_INFO,
            Self::Warn => COLOR_WARNING,
            Self::Danger => COLOR_ERROR,
            Self::Ok => COLOR_SUCCESS,
        }
    }
}

fn container(tone: Tone, children: Vec<Component>) -> Message {
    Message::new(vec![Component::Container {
        accent: Some(tone.color()),
        children,
    }])
}

/// What a card needs to know about one track.
#[derive(Debug, Clone)]
pub struct TrackCard<'a> {
    pub title: &'a str,
    pub url: &'a str,
    pub artists: &'a [String],
    pub duration_ms: Option<u64>,
    pub thumbnail: Option<&'a str>,
    pub requester_id: u64,
    /// Picked by the AI radio: the meta line adds "Selected by AI".
    pub ai: bool,
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

    fn link(&self, max: usize) -> String {
        link(&truncate(self.title, max), self.url)
    }

    /// `1. [Title](url) · Artist `3:07``
    fn list_line(&self, pos: usize, i18n: &I18n) -> String {
        let mut line = format!("`{pos}.` {}", self.link(60));
        if !self.artists.is_empty() {
            line.push_str(&format!(" · {}", escape(&truncate(&self.artists.join(", "), 40))));
        }
        line.push_str(&format!(" `{}`", self.duration(i18n)));
        line
    }
}

fn channel(id: u64) -> String {
    format!("<#{id}>")
}

/// A jam channel shown by name (it may be in another server, where a mention does not resolve).
fn jam_name(name: &str) -> String {
    escape(&truncate(name, 80))
}

fn user(id: u64) -> String {
    format!("<@{id}>")
}

fn panel_button(voice_channel_id: u64, i18n: &I18n) -> Button {
    Button::new(
        ButtonStyle::Primary,
        i18n.t("bot.button.panel", &[]),
        CustomId::np(NpAction::Panel, voice_channel_id, NO_TRACK),
    )
    .icon(Icon::Panel)
}

/// A progress bar such as `1:07 ━━━━━●───── 3:34`.
pub fn progress_bar(position_ms: u64, duration_ms: Option<u64>) -> String {
    match duration_ms.filter(|d| *d > 0) {
        Some(d) => {
            let filled = ((position_ms.min(d) as f64 / d as f64) * (BAR_CELLS - 1) as f64).round() as usize;
            format!(
                "`{}` {}\u{25CF}{} `{}`",
                format_duration(position_ms),
                "\u{2501}".repeat(filled),
                "\u{2500}".repeat(BAR_CELLS - 1 - filled),
                format_duration(d)
            )
        }
        None => format!("`{}`", format_duration(position_ms)),
    }
}

// ---------------------------------------------------------------------------
// Added to the queue
// ---------------------------------------------------------------------------

/// `position` 0 = starts now. `eta_ms`: time until it plays, when known.
pub fn added_single(track: &TrackCard, position: usize, eta_ms: Option<u64>, i18n: &I18n) -> Message {
    let mut meta = vec![track.duration(i18n)];
    if position == 0 {
        meta.push(i18n.t("bot.added.playing_now", &[]));
    } else {
        meta.push(i18n.t("bot.added.position", &[("position", &position.to_string())]));
        if let Some(eta) = eta_ms.filter(|e| *e >= 60_000) {
            meta.push(i18n.t("bot.added.eta", &[("minutes", &(eta / 60_000).to_string())]));
        }
    }
    let mut texts = vec![
        format!("-# {}", i18n.t("bot.added.title", &[])),
        format!("### {}", track.link(200)),
    ];
    if !track.artists.is_empty() {
        texts.push(track.artists());
    }
    texts.push(format!("-# {}", meta.join(" · ")));
    container(Tone::Info, Component::section_or_text(texts, track.thumbnail))
}

pub struct AddedList<'a> {
    pub name: Option<&'a str>,
    /// Link of the playlist itself, when it is a URL.
    pub url: Option<&'a str>,
    pub origin: TrackOrigin,
    pub count: usize,
    pub total_ms: u64,
    pub thumbnail: Option<&'a str>,
    /// The first added tracks with their queue positions (at most `PLAYLIST_PREVIEW`).
    pub preview: Vec<(usize, TrackCard<'a>)>,
    pub truncated: bool,
    pub voice_channel_id: u64,
}

pub fn added_list(v: &AddedList, i18n: &I18n) -> Message {
    let count = v.count.to_string();
    let headline = match v.name {
        Some(n) => {
            let name = match v.url {
                Some(u) => link(n, u),
                None => escape(&truncate(n, 100)),
            };
            i18n.t("bot.added.list_named", &[("count", &count), ("name", &name)])
        }
        None => i18n.t("bot.added.list", &[("count", &count)]),
    };
    let source = match v.origin {
        TrackOrigin::Spotify => i18n.t("bot.added.source_spotify", &[]),
        TrackOrigin::Youtube => i18n.t("bot.added.source_youtube", &[]),
        TrackOrigin::YoutubeMusic => i18n.t("bot.added.source_youtube_music", &[]),
        TrackOrigin::AppleMusic | TrackOrigin::Other => i18n.t("bot.added.source_other", &[]),
    };
    let mut meta = vec![source];
    if v.total_ms > 0 {
        meta.push(i18n.t("bot.added.total", &[("duration", &format_duration(v.total_ms))]));
    }
    let texts = vec![
        format!("-# {}", i18n.t("bot.added.list_title", &[])),
        format!("### {headline}"),
        format!("-# {}", meta.join(" · ")),
    ];
    let mut children = Component::section_or_text(texts, v.thumbnail);
    children.push(Component::separator());
    let mut lines: Vec<String> = v.preview.iter().map(|(pos, t)| t.list_line(*pos, i18n)).collect();
    if v.count > v.preview.len() {
        lines.push(format!(
            "-# {}",
            i18n.t("bot.added.more", &[("count", &(v.count - v.preview.len()).to_string())])
        ));
    }
    if v.truncated {
        lines.push(format!("-# {}", i18n.t("bot.added.truncated", &[])));
    }
    children.push(Component::text(lines.join("\n")));
    children.push(Component::Row(vec![
        Button::new(
            ButtonStyle::Secondary,
            i18n.t("bot.button.queue", &[]),
            CustomId::np(NpAction::Queue, v.voice_channel_id, NO_TRACK),
        )
        .icon(Icon::Queue),
        panel_button(v.voice_channel_id, i18n),
    ]));
    container(Tone::Info, children)
}

// ---------------------------------------------------------------------------
// Now playing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct NowPlaying<'a> {
    pub track: TrackCard<'a>,
    pub voice_channel_id: u64,
    /// Discord display name of the extra bot playing it (None on the main bot).
    pub node_name: Option<&'a str>,
    /// The channel plays a jam.
    pub jam: Option<&'a JamLine>,
    pub genres: &'a [String],
    pub paused: bool,
    pub ai_enabled: bool,
    /// `ids::track_tag` of the entry.
    pub track_tag: &'a str,
}

/// The jam line of the now-playing header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JamLine {
    /// Name of the host voice channel (may be in another server, so not a mention).
    pub host_name: String,
    pub channels: usize,
    /// This channel is the host.
    pub hosting: bool,
}

fn header(voice_channel_id: u64, node_name: Option<&str>, jam: Option<&JamLine>, i18n: &I18n) -> String {
    let mut h = i18n.t("bot.now_playing.title_in", &[("channel", &channel(voice_channel_id))]);
    if let Some(node) = node_name {
        h.push_str(" · ");
        h.push_str(&i18n.t("bot.now_playing.played_by", &[("bot", &escape(node))]));
    }
    if let Some(j) = jam {
        h.push_str(" · ");
        let count = j.channels.to_string();
        h.push_str(&if j.hosting {
            i18n.t("bot.now_playing.jam_host", &[("count", &count)])
        } else {
            i18n.t(
                "bot.now_playing.jam_with",
                &[("channel", &escape(&truncate(&j.host_name, 80))), ("count", &count)],
            )
        });
    }
    format!("-# {h}")
}

fn toggle_button(paused: bool, vc: u64, tag: &str, i18n: &I18n) -> Button {
    if paused {
        Button::new(
            ButtonStyle::Secondary,
            i18n.t("bot.button.resume", &[]),
            CustomId::np(NpAction::Resume, vc, tag),
        )
        .icon(Icon::Play)
    } else {
        Button::new(
            ButtonStyle::Secondary,
            i18n.t("bot.button.pause", &[]),
            CustomId::np(NpAction::Pause, vc, tag),
        )
        .icon(Icon::Pause)
    }
}

fn skip_button(vc: u64, tag: &str, i18n: &I18n) -> Button {
    Button::new(
        ButtonStyle::Secondary,
        i18n.t("bot.button.skip", &[]),
        CustomId::np(NpAction::Skip, vc, tag),
    )
    .icon(Icon::Skip)
}

fn queue_button(vc: u64, tag: &str, i18n: &I18n) -> Button {
    Button::new(
        ButtonStyle::Secondary,
        i18n.t("bot.button.queue", &[]),
        CustomId::np(NpAction::Queue, vc, tag),
    )
    .icon(Icon::Queue)
}

/// The automatic message sent when a track starts (one per player; replaced on every track).
pub fn now_playing(v: &NowPlaying, i18n: &I18n) -> Message {
    let vc = v.voice_channel_id;
    let mut texts = vec![header(vc, v.node_name, v.jam, i18n), format!("### {}", v.track.link(200))];
    if !v.track.artists.is_empty() {
        texts.push(v.track.artists());
    }
    let mut meta = vec![
        v.track.duration(i18n),
        i18n.t("bot.track.requested_by", &[("user", &user(v.track.requester_id))]),
    ];
    if v.track.ai {
        meta.push(i18n.t("bot.track.selected_by_ai", &[]));
    }
    if v.paused {
        meta.push(i18n.t("bot.now_playing.paused", &[]));
    }
    let mut last = format!("-# {}", meta.join(" · "));
    if !v.genres.is_empty() {
        let chips = v
            .genres
            .iter()
            .take(4)
            .map(|g| format!("`{}`", g.replace('`', "")))
            .collect::<Vec<_>>();
        last.push('\n');
        last.push_str(&chips.join(" "));
    }
    texts.push(last);
    let mut children = Component::section_or_text(texts, v.track.thumbnail);
    children.push(Component::separator());

    let tag = v.track_tag;
    children.push(Component::Row(vec![
        // Song controls are grey; their colored icons tell them apart.
        toggle_button(v.paused, vc, tag, i18n),
        Button::new(
            ButtonStyle::Secondary,
            i18n.t("bot.button.stop", &[]),
            CustomId::np(NpAction::Stop, vc, tag),
        )
        .icon(Icon::Stop),
        skip_button(vc, tag, i18n),
        queue_button(vc, tag, i18n),
    ]));
    let mut second = Vec::with_capacity(4);
    if v.ai_enabled {
        second.push(
            Button::new(
                ButtonStyle::Success,
                i18n.t("bot.button.like", &[]),
                CustomId::np(NpAction::Like, vc, tag),
            )
            .icon(Icon::Like),
        );
        second.push(
            Button::new(
                ButtonStyle::Danger,
                i18n.t("bot.button.dislike", &[]),
                CustomId::np(NpAction::Dislike, vc, tag),
            )
            .icon(Icon::Dislike),
        );
    }
    second.push(
        Button::new(
            ButtonStyle::Secondary,
            i18n.t("bot.button.jam", &[]),
            CustomId::np(NpAction::Jam, vc, tag),
        )
        .icon(Icon::Jam),
    );
    second.push(panel_button(vc, i18n));
    children.push(Component::Row(second));
    container(Tone::Info, children)
}

#[derive(Debug, Clone)]
pub struct NowPlayingDetail<'a> {
    pub track: TrackCard<'a>,
    pub voice_channel_id: u64,
    pub node_name: Option<&'a str>,
    pub jam: Option<&'a JamLine>,
    pub position_ms: u64,
    pub paused: bool,
    pub loop_mode: LoopMode,
    pub volume: u16,
    pub track_tag: &'a str,
}

/// `/nowplaying`: progress bar, loop and volume.
pub fn now_playing_detail(v: &NowPlayingDetail, i18n: &I18n) -> Message {
    let vc = v.voice_channel_id;
    let mut texts = vec![header(vc, v.node_name, v.jam, i18n), format!("### {}", v.track.link(200))];
    let mut body = String::new();
    if !v.track.artists.is_empty() {
        body.push_str(&v.track.artists());
        body.push('\n');
    }
    body.push_str(&progress_bar(v.position_ms, v.track.duration_ms));
    texts.push(body);
    let mut meta = vec![i18n.t("bot.track.requested_by", &[("user", &user(v.track.requester_id))])];
    if v.track.ai {
        meta.push(i18n.t("bot.track.selected_by_ai", &[]));
    }
    meta.extend([
        loop_label(v.loop_mode, i18n),
        i18n.t("bot.now_playing.volume", &[("volume", &v.volume.to_string())]),
    ]);
    if v.paused {
        meta.push(i18n.t("bot.now_playing.paused", &[]));
    }
    texts.push(format!("-# {}", meta.join(" · ")));
    let mut children = Component::section_or_text(texts, v.track.thumbnail);
    children.push(Component::separator());
    let tag = v.track_tag;
    children.push(Component::Row(vec![
        toggle_button(v.paused, vc, tag, i18n),
        skip_button(vc, tag, i18n),
        queue_button(vc, tag, i18n),
        panel_button(vc, i18n),
    ]));
    container(Tone::Info, children)
}

pub fn loop_label(mode: LoopMode, i18n: &I18n) -> String {
    i18n.t("bot.loop.label", &[("mode", &loop_mode_name(mode, i18n))])
}

pub fn loop_mode_name(mode: LoopMode, i18n: &I18n) -> String {
    match mode {
        LoopMode::Off => i18n.t("bot.loop.off", &[]),
        LoopMode::Track => i18n.t("bot.loop.track", &[]),
        LoopMode::Queue => i18n.t("bot.loop.queue", &[]),
    }
}

// ---------------------------------------------------------------------------
// Queue
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct QueuePage<'a> {
    pub voice_channel_id: u64,
    /// Current track and its position.
    pub current: Option<(TrackCard<'a>, u64)>,
    /// The tracks of this page only, with their 1-based positions in the upcoming list.
    pub entries: Vec<(usize, TrackCard<'a>)>,
    /// 0-based.
    pub page: u32,
    pub total_upcoming: usize,
    /// Time left: rest of the current track plus every upcoming track.
    pub remaining_ms: u64,
    pub loop_mode: LoopMode,
    /// Unix seconds; the page buttons expire after `ids::QUEUE_BUTTONS_TTL_SECS`.
    pub issued: i64,
    /// Render without the page buttons (expired).
    pub expired: bool,
}

pub fn page_count(total: usize) -> u32 {
    total.div_ceil(QUEUE_PAGE_SIZE).max(1) as u32
}

pub fn queue_page(v: &QueuePage, i18n: &I18n) -> Message {
    let pages = page_count(v.total_upcoming);
    let vc = v.voice_channel_id;
    let mut texts = vec![format!(
        "### {}\n-# {}",
        i18n.t("bot.queue.title", &[("channel", &channel(vc))]),
        i18n.t(
            "bot.queue.summary",
            &[
                ("count", &v.total_upcoming.to_string()),
                ("duration", &format_duration(v.remaining_ms))
            ]
        )
    )];
    if let Some((cur, pos)) = &v.current {
        let mut line = format!("**{}** {}", i18n.t("bot.queue.now", &[]), cur.link(80));
        if !cur.artists.is_empty() {
            line.push_str(&format!(" · {}", escape(&truncate(&cur.artists.join(", "), 40))));
        }
        let time = match cur.duration_ms.filter(|d| *d > 0) {
            Some(d) => format!("{} / {}", format_duration(*pos), format_duration(d)),
            None => i18n.t("bot.track.live", &[]),
        };
        line.push_str(&format!(" `{time}`"));
        texts.push(line);
    }
    let mut children = Component::section_or_text(texts, v.current.as_ref().and_then(|(c, _)| c.thumbnail));
    children.push(Component::separator());
    if v.entries.is_empty() {
        children.push(Component::text(i18n.t("bot.queue.empty", &[])));
    } else {
        let lines: Vec<String> = v.entries.iter().map(|(pos, t)| t.list_line(*pos, i18n)).collect();
        children.push(Component::text(lines.join("\n")));
    }
    children.push(Component::separator());
    children.push(Component::text(format!(
        "-# {} · {}",
        i18n.t(
            "bot.queue.page",
            &[("page", &(v.page + 1).to_string()), ("pages", &pages.to_string())]
        ),
        loop_label(v.loop_mode, i18n)
    )));
    let mut row = Vec::new();
    if pages > 1 && !v.expired {
        let id = |page| {
            CustomId::QueuePage {
                voice_channel_id: vc,
                page,
                issued: v.issued,
            }
            .encode()
        };
        row.push(
            Button::new(
                ButtonStyle::Secondary,
                i18n.t("bot.button.previous", &[]),
                id(v.page.saturating_sub(1)),
            )
            .icon(Icon::Previous)
            .disabled(v.page == 0),
        );
        row.push(
            Button::new(ButtonStyle::Secondary, i18n.t("bot.button.next", &[]), id(v.page + 1))
                .icon(Icon::Next)
                .disabled(v.page + 1 >= pages),
        );
    }
    row.push(panel_button(vc, i18n));
    children.push(Component::Row(row));
    container(Tone::Info, children)
}

// ---------------------------------------------------------------------------
// Lyrics and jam
// ---------------------------------------------------------------------------

/// Characters of lyrics shown (after escaping), under the 4000 of a text display.
const LYRICS_MAX: usize = 3200;

/// Lyrics from LRCLIB (anyone can submit them) as plain text: every line escaped ([`escape_line`]:
/// no markdown, masked links, mentions or headings), whole lines up to `max` characters, then `...`.
fn lyrics_text(body: &str, max: usize) -> String {
    let mut text = String::new();
    let mut len = 0;
    for line in body.trim().lines() {
        let line = escape_line(line);
        let n = line.chars().count() + 1;
        if len + n > max {
            if text.is_empty() {
                text = truncate(&line, max);
            }
            text.push_str("\n...");
            return text;
        }
        if len > 0 {
            text.push('\n');
        }
        text.push_str(&line);
        len += n;
    }
    text
}

pub fn lyrics(track: &TrackCard, body: &str, voice_channel_id: Option<u64>, i18n: &I18n) -> Message {
    let mut texts = vec![
        format!("-# {}", i18n.t("bot.lyrics.title", &[])),
        format!("### {}", escape(&truncate(&sanitize_text(track.title), 150))),
    ];
    if !track.artists.is_empty() {
        texts.push(track.artists());
    }
    let mut children = Component::section_or_text(texts, track.thumbnail);
    children.push(Component::separator());
    children.push(Component::text(lyrics_text(body, LYRICS_MAX)));
    children.push(Component::separator());
    children.push(Component::text(format!("-# {}", i18n.t("bot.lyrics.footer", &[]))));
    children.push(Component::Row(vec![panel_button(voice_channel_id.unwrap_or(0), i18n)]));
    container(Tone::Info, children)
}

/// Ephemeral reply to "Invite to jam": the jam's Activity link (opens the panel in the clicker's
/// own voice channel, where they can join) and, when it could be made, a plain invite into this
/// channel as a fallback.
pub fn jam(voice_channel_id: u64, share_link: &str, invite_code: Option<&str>, i18n: &I18n) -> Message {
    let ch = channel(voice_channel_id);
    // Plain URLs: Discord prints a masked link whose label looks like a URL raw.
    let mut texts = vec![
        format!("**{}**", i18n.t("bot.jam.title", &[("channel", &ch)])),
        i18n.t("bot.jam.body", &[]),
        share_link.to_owned(),
    ];
    if let Some(code) = invite_code {
        texts[2].push_str(&format!(
            "
-# {}",
            i18n.t(
                "bot.jam.invite",
                &[("channel", &ch), ("url", &format!("https://discord.gg/{code}"))]
            )
        ));
    }
    let accessory = Accessory::Button(Button::link(i18n.t("bot.button.open_jam", &[]), share_link));
    container(Tone::Ok, vec![Component::Section { texts, accessory }]).ephemeral(true)
}

// ---------------------------------------------------------------------------
// Automatic notices (one status message per player replaces the previous one)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum Notice<'a> {
    PausedEmpty {
        deadline_secs: i64,
    },
    Resumed {
        title: &'a str,
        url: &'a str,
    },
    QueueDeleted {
        minutes: u64,
    },
    BackOnline {
        title: &'a str,
        url: &'a str,
        position: usize,
        total: usize,
    },
    QueueFinished {
        ai_enabled: bool,
    },
    TrackFailed {
        title: &'a str,
        url: &'a str,
        reason: &'a str,
    },
    TooManyFailures,
    Disconnected,
    Stopped {
        by: u64,
    },
    /// This channel joined the jam of `host`.
    JamEntered {
        host: &'a str,
    },
    /// Another channel joined (`count` channels now).
    JamJoined {
        channel: &'a str,
        count: usize,
    },
    JamLeft {
        channel: &'a str,
        count: usize,
    },
    /// The host role moved to `channel` (`own`: to this channel).
    JamHost {
        channel: &'a str,
        own: bool,
    },
    /// `restored`: the own queue came back (otherwise the music goes on here).
    JamEnded {
        restored: bool,
    },
    /// The host removed this channel.
    JamRemoved,
    /// This channel left the jam (`restored`: its own queue came back).
    JamLeftOwn {
        restored: bool,
    },
    /// Nobody is left in this jam channel: it leaves the jam and disconnects at `deadline_secs`.
    JamEmpty {
        deadline_secs: i64,
    },
    /// Someone came back to this jam channel before the deadline.
    JamBack,
}

fn titled(title: String, body: String) -> Vec<Component> {
    vec![Component::text(format!("**{title}**\n{body}"))]
}

fn titled_accessory(title: String, body: String, button: Button) -> Vec<Component> {
    vec![Component::Section {
        texts: vec![format!("**{title}**"), body],
        accessory: Accessory::Button(button),
    }]
}

pub fn notice(n: &Notice, voice_channel_id: u64, i18n: &I18n) -> Message {
    let ch = channel(voice_channel_id);
    let (tone, children) = match n {
        Notice::PausedEmpty { deadline_secs } => (
            Tone::Warn,
            titled(
                i18n.t("bot.notice.paused_empty.title", &[]),
                i18n.t(
                    "bot.notice.paused_empty.body",
                    &[("channel", &ch), ("time", &relative_time(*deadline_secs))],
                ),
            ),
        ),
        Notice::Resumed { title, url } => (
            Tone::Ok,
            titled(
                i18n.t("bot.notice.resumed.title", &[]),
                i18n.t(
                    "bot.notice.resumed.body",
                    &[("channel", &ch), ("track", &link(&truncate(title, 80), url))],
                ),
            ),
        ),
        Notice::QueueDeleted { minutes } => (
            Tone::Danger,
            titled_accessory(
                i18n.t("bot.notice.queue_deleted.title", &[]),
                i18n.t(
                    "bot.notice.queue_deleted.body",
                    &[("channel", &ch), ("minutes", &minutes.to_string())],
                ),
                Button::new(
                    ButtonStyle::Secondary,
                    i18n.t("bot.button.replay", &[]),
                    CustomId::Replay { voice_channel_id }.encode(),
                )
                .icon(Icon::Replay),
            ),
        ),
        Notice::BackOnline {
            title,
            url,
            position,
            total,
        } => (
            Tone::Info,
            titled(
                i18n.t("bot.notice.back_online.title", &[]),
                i18n.t(
                    "bot.notice.back_online.body",
                    &[
                        ("channel", &ch),
                        ("track", &link(&truncate(title, 80), url)),
                        ("position", &position.to_string()),
                        ("total", &total.to_string()),
                    ],
                ),
            ),
        ),
        Notice::QueueFinished { ai_enabled: true } => (
            Tone::Info,
            titled_accessory(
                i18n.t("bot.notice.queue_finished.title", &[]),
                i18n.t("bot.notice.queue_finished.body_ai", &[("channel", &ch)]),
                Button::new(
                    ButtonStyle::Primary,
                    i18n.t("bot.button.start_radio", &[]),
                    CustomId::StartRadio { voice_channel_id }.encode(),
                )
                .icon(Icon::Radio),
            ),
        ),
        Notice::QueueFinished { ai_enabled: false } => (
            Tone::Info,
            titled(
                i18n.t("bot.notice.queue_finished.title", &[]),
                i18n.t("bot.notice.queue_finished.body", &[("channel", &ch)]),
            ),
        ),
        Notice::TrackFailed { title, url, reason } => (
            Tone::Danger,
            titled(
                i18n.t(
                    "bot.notice.track_failed.title",
                    &[("track", &link(&truncate(title, 80), url))],
                ),
                i18n.t(
                    "bot.notice.track_failed.body",
                    &[("reason", &escape(&truncate(reason, 200)))],
                ),
            ),
        ),
        Notice::TooManyFailures => (
            Tone::Danger,
            titled(
                i18n.t("bot.notice.too_many_failures.title", &[]),
                i18n.t("bot.notice.too_many_failures.body", &[("channel", &ch)]),
            ),
        ),
        Notice::Disconnected => (
            Tone::Danger,
            titled(
                i18n.t("bot.notice.disconnected.title", &[]),
                i18n.t("bot.notice.disconnected.body", &[("channel", &ch)]),
            ),
        ),
        Notice::Stopped { by } => (
            Tone::Info,
            titled(
                i18n.t("bot.notice.stopped.title", &[]),
                i18n.t("bot.notice.stopped.body", &[("channel", &ch), ("user", &user(*by))]),
            ),
        ),
        Notice::JamEntered { host } => (
            Tone::Ok,
            titled(
                i18n.t("bot.notice.jam_entered.title", &[("host", &jam_name(host))]),
                i18n.t("bot.notice.jam_entered.body", &[("channel", &ch)]),
            ),
        ),
        Notice::JamJoined { channel, count } => (
            Tone::Info,
            titled(
                i18n.t("bot.notice.jam_joined.title", &[("channel", &jam_name(channel))]),
                i18n.t("bot.notice.jam_joined.body", &[("count", &count.to_string())]),
            ),
        ),
        Notice::JamLeft { channel, count } => (
            Tone::Info,
            titled(
                i18n.t("bot.notice.jam_left.title", &[("channel", &jam_name(channel))]),
                i18n.t("bot.notice.jam_left.body", &[("count", &count.to_string())]),
            ),
        ),
        Notice::JamHost { own: true, .. } => (
            Tone::Info,
            titled(
                i18n.t("bot.notice.jam_host.title_own", &[("channel", &ch)]),
                i18n.t("bot.notice.jam_host.body_own", &[]),
            ),
        ),
        Notice::JamHost { channel, own: false } => (
            Tone::Info,
            titled(
                i18n.t("bot.notice.jam_host.title", &[("channel", &jam_name(channel))]),
                i18n.t("bot.notice.jam_host.body", &[]),
            ),
        ),
        Notice::JamEnded { restored } => (
            Tone::Info,
            titled(
                i18n.t("bot.notice.jam_ended.title", &[]),
                i18n.t(
                    if *restored {
                        "bot.notice.jam_ended.body_restored"
                    } else {
                        "bot.notice.jam_ended.body"
                    },
                    &[("channel", &ch)],
                ),
            ),
        ),
        Notice::JamRemoved => (
            Tone::Warn,
            titled(
                i18n.t("bot.notice.jam_removed.title", &[]),
                i18n.t("bot.notice.jam_removed.body", &[("channel", &ch)]),
            ),
        ),
        Notice::JamLeftOwn { restored } => (
            Tone::Info,
            titled(
                i18n.t("bot.notice.jam_left_own.title", &[("channel", &ch)]),
                i18n.t(
                    if *restored {
                        "bot.notice.jam_ended.body_restored"
                    } else {
                        "bot.notice.jam_ended.body"
                    },
                    &[("channel", &ch)],
                ),
            ),
        ),
        Notice::JamEmpty { deadline_secs } => (
            Tone::Warn,
            titled(
                i18n.t("bot.notice.jam_empty.title", &[]),
                i18n.t(
                    "bot.notice.jam_empty.body",
                    &[("channel", &ch), ("time", &relative_time(*deadline_secs))],
                ),
            ),
        ),
        Notice::JamBack => (
            Tone::Ok,
            titled(
                i18n.t("bot.notice.jam_back.title", &[]),
                i18n.t("bot.notice.jam_back.body", &[("channel", &ch)]),
            ),
        ),
    };
    container(tone, children)
}

// ---------------------------------------------------------------------------
// Replies
// ---------------------------------------------------------------------------

/// Public one-line confirmation (`**Paused.** Use /resume ...`).
pub fn confirm(text: impl Into<String>) -> Message {
    container(Tone::Info, vec![Component::text(text)])
}

/// Ephemeral confirmation for personal actions (feedback).
pub fn confirm_private(tone: Tone, text: impl Into<String>) -> Message {
    container(tone, vec![Component::text(text)]).ephemeral(true)
}

/// Ephemeral error: bold title plus an explanation.
pub fn error(title: impl Into<String>, body: impl Into<String>) -> Message {
    let (title, body) = (title.into(), body.into());
    let text = if body.is_empty() {
        format!("**{title}**")
    } else {
        format!("**{title}** {body}")
    };
    container(Tone::Danger, vec![Component::text(text)]).ephemeral(true)
}

/// Error from an i18n pair `bot.error.<key>.title` / `.body`.
pub fn error_key(key: &str, args: &[(&str, &str)], i18n: &I18n) -> Message {
    error(
        i18n.t(&format!("bot.error.{key}.title"), args),
        i18n.t(&format!("bot.error.{key}.body"), args),
    )
}

/// Vote to skip in progress, with a button to add a vote.
pub fn vote(
    title: &str,
    url: &str,
    current: usize,
    required: usize,
    voice_channel_id: u64,
    tag: &str,
    i18n: &I18n,
) -> Message {
    let missing = required.saturating_sub(current).max(1);
    let body_key = if missing == 1 {
        "bot.vote.body_one"
    } else {
        "bot.vote.body_many"
    };
    container(
        Tone::Info,
        vec![
            Component::text(format!(
                "**{}**\n{}",
                i18n.t(
                    "bot.vote.title",
                    &[("current", &current.to_string()), ("required", &required.to_string())]
                ),
                i18n.t(
                    body_key,
                    &[
                        ("track", &link(&truncate(title, 80), url)),
                        ("count", &missing.to_string())
                    ]
                )
            )),
            Component::Row(vec![Button::new(
                ButtonStyle::Secondary,
                i18n.t("bot.button.vote", &[]),
                CustomId::np(NpAction::Skip, voice_channel_id, tag),
            )
            .icon(Icon::Skip)]),
        ],
    )
}

/// Every Lynx bot is busy in the guild. `invite` = (node number, invite URL) of the next node to add.
pub fn all_busy(busy_channel: Option<u64>, target_channel: u64, invite: Option<(usize, &str)>, i18n: &I18n) -> Message {
    let target = channel(target_channel);
    let title = i18n.t("bot.busy.title", &[]);
    let body = match (invite, busy_channel) {
        (None, _) => i18n.t("bot.busy.no_more", &[("channel", &target)]),
        (Some(_), Some(busy)) => i18n.t("bot.busy.body", &[("busy", &channel(busy)), ("channel", &target)]),
        (Some(_), None) => i18n.t("bot.busy.body_generic", &[("channel", &target)]),
    };
    let children = match invite {
        Some((_, url)) => titled_accessory(
            title,
            body,
            Button::link(i18n.t("bot.button.invite_node", &[]), url),
        ),
        None => titled(title, body),
    };
    container(Tone::Warn, children).ephemeral(true)
}

/// Reply of every node command in a guild without the main bot.
pub fn add_main(invite_url: &str, i18n: &I18n) -> Message {
    container(
        Tone::Warn,
        vec![
            Component::text(format!(
                "**{}**\n{}",
                i18n.t("bot.add_main.title", &[]),
                i18n.t("bot.add_main.body", &[])
            )),
            Component::Row(vec![Button::link(i18n.t("bot.button.invite_main", &[]), invite_url)]),
        ],
    )
    .ephemeral(true)
}

/// Like / Dislike from a user whose history is turned off.
pub fn history_off(voice_channel_id: u64, i18n: &I18n) -> Message {
    container(
        Tone::Warn,
        vec![
            Component::text(format!(
                "**{}**\n{}",
                i18n.t("bot.history_off.title", &[]),
                i18n.t("bot.history_off.body", &[])
            )),
            Component::Row(vec![panel_button(voice_channel_id, i18n)]),
        ],
    )
    .ephemeral(true)
}

// ---------------------------------------------------------------------------
// AI radio
// ---------------------------------------------------------------------------

/// State of one phase line on the "picking songs" card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Done,
    Working,
    Next,
}

pub fn phase_label(phase: AiPhase, i18n: &I18n) -> String {
    let key = match phase {
        AiPhase::Queued => "queued",
        AiPhase::LoadingModel => "loading_model",
        AiPhase::ReadingTaste => "reading_taste",
        AiPhase::FindingSongs => "finding_songs",
        AiPhase::Thinking => "thinking",
        _ => "resolving",
    };
    i18n.t(&format!("bot.radio.phase.{key}"), &[])
}

pub struct RadioPicking<'a> {
    pub voice_channel_id: u64,
    pub prompt: Option<&'a str>,
    pub first_playing: bool,
    pub steps: &'a [(AiPhase, Step)],
    /// Job uuid (simple form) for the Cancel button.
    pub job: &'a str,
}

/// "Radio is picking songs": the phase list, edited as the job advances.
pub fn radio_picking(v: &RadioPicking, i18n: &I18n) -> Message {
    let mut sub = match v.prompt {
        Some(p) => i18n.t(
            "bot.radio.picking.basis_prompt",
            &[("prompt", &escape(&truncate(p, 100)))],
        ),
        None => i18n.t("bot.radio.picking.basis", &[]),
    };
    if v.first_playing {
        sub.push(' ');
        sub.push_str(&i18n.t("bot.radio.picking.first_playing", &[]));
    }
    let lines: Vec<String> = v
        .steps
        .iter()
        .map(|(phase, step)| {
            let badge = match step {
                Step::Done => i18n.t("bot.radio.badge.done", &[]),
                Step::Working => format!("**{}**", i18n.t("bot.radio.badge.working", &[])),
                Step::Next => i18n.t("bot.radio.badge.next", &[]),
            };
            let label = phase_label(*phase, i18n);
            match step {
                Step::Working => format!("{badge} - **{label}**"),
                _ => format!("{badge} - {label}"),
            }
        })
        .collect();
    container(
        Tone::Info,
        vec![
            Component::text(format!(
                "**{}**
-# {sub}",
                i18n.t("bot.radio.picking.title", &[])
            )),
            Component::separator(),
            Component::text(lines.join(
                "
",
            )),
            Component::Row(vec![
                Button::new(
                    ButtonStyle::Danger,
                    i18n.t("bot.button.cancel", &[]),
                    CustomId::CancelRadio {
                        voice_channel_id: v.voice_channel_id,
                        job: v.job.to_owned(),
                    }
                    .encode(),
                )
                .icon(Icon::Cancel),
                panel_button(v.voice_channel_id, i18n),
            ]),
        ],
    )
}

/// "Radio is on" with the number of songs the start job added.
pub fn radio_on(count: usize, voice_channel_id: u64, i18n: &I18n) -> Message {
    let ch = channel(voice_channel_id);
    let body = if count == 1 {
        i18n.t("bot.radio.on.body_one", &[("channel", &ch)])
    } else {
        i18n.t("bot.radio.on.body", &[("count", &count.to_string()), ("channel", &ch)])
    };
    container(
        Tone::Ok,
        vec![
            Component::text(format!(
                "**{}**
{body}",
                i18n.t("bot.radio.on.title", &[])
            )),
            Component::Row(vec![
                Button::new(
                    ButtonStyle::Secondary,
                    i18n.t("bot.button.stop_radio", &[]),
                    CustomId::StopRadio { voice_channel_id }.encode(),
                )
                .icon(Icon::Radio),
                panel_button(voice_channel_id, i18n),
            ]),
        ],
    )
}

/// The start job failed before adding anything.
pub fn radio_failed(no_songs: bool, i18n: &I18n) -> Message {
    let body = if no_songs {
        i18n.t("bot.radio.failed.no_songs", &[])
    } else {
        i18n.t("bot.radio.failed.body", &[])
    };
    container(Tone::Danger, titled(i18n.t("bot.radio.failed.title", &[]), body))
}

pub fn radio_cancelled(voice_channel_id: u64, i18n: &I18n) -> Message {
    container(
        Tone::Info,
        titled(
            i18n.t("bot.radio.cancelled.title", &[]),
            i18n.t("bot.radio.cancelled.body", &[("channel", &channel(voice_channel_id))]),
        ),
    )
}

/// Replaces the "Radio is on" card after "Stop radio".
pub fn radio_stopped(by: u64, i18n: &I18n) -> Message {
    container(
        Tone::Info,
        titled(
            i18n.t("bot.radio.stopped.title", &[]),
            i18n.t("bot.radio.stopped.body", &[("user", &user(by))]),
        ),
    )
}

/// Public notice for a guild's system channel (node bot joined a guild without the main bot).
pub fn node_notice(invite_url: &str, i18n: &I18n) -> Message {
    add_main(invite_url, i18n).ephemeral(false)
}

// ---------------------------------------------------------------------------
// Invite and about
// ---------------------------------------------------------------------------

/// Link buttons to the support server and the website, when they are configured.
fn info_links(support_url: Option<&str>, website_url: Option<&str>, i18n: &I18n) -> Vec<Button> {
    let ok = |u: &&str| u.starts_with("https://") || u.starts_with("http://");
    let mut out = Vec::new();
    if let Some(u) = support_url.filter(ok) {
        out.push(Button::link(i18n.t("bot.button.support", &[]), u));
    }
    if let Some(u) = website_url.filter(ok) {
        out.push(Button::link(i18n.t("bot.button.website", &[]), u));
    }
    out
}

/// Ephemeral reply to `/invite`.
pub fn invite(invite_url: &str, support_url: Option<&str>, website_url: Option<&str>, i18n: &I18n) -> Message {
    let mut row = vec![Button::link(i18n.t("bot.button.invite_main", &[]), invite_url)];
    row.extend(info_links(support_url, website_url, i18n));
    container(
        Tone::Info,
        vec![
            Component::text(format!(
                "**{}**
{}",
                i18n.t("bot.invite.title", &[]),
                i18n.t("bot.invite.body", &[])
            )),
            Component::Row(row),
        ],
    )
    .ephemeral(true)
}

/// Everything `/about` shows.
#[derive(Debug, Clone)]
pub struct About<'a> {
    pub version: &'a str,
    pub commit: Option<&'a str>,
    /// The main bot's avatar.
    pub avatar: Option<&'a str>,
    pub uptime: std::time::Duration,
    /// Gateway heartbeat latency; None until the first heartbeat is acknowledged.
    pub ping: Option<std::time::Duration>,
    pub servers: usize,
    /// Every Lynx bot in this process.
    pub bots: usize,
    /// Voice channels with a player, and how many of them are playing right now.
    pub voice_channels: usize,
    pub playing: usize,
    pub memory_bytes: Option<u64>,
    pub ytdlp: YtdlpLine,
    pub invite_url: &'a str,
    pub support_url: Option<&'a str>,
    pub website_url: Option<&'a str>,
}

/// `3d 4h 12m`, `4h 12m`, `12m 5s`, `5s`.
pub fn format_uptime(d: std::time::Duration) -> String {
    let s = d.as_secs();
    let (days, h, m, sec) = (s / 86_400, (s / 3600) % 24, (s / 60) % 60, s % 60);
    match (days, h, m) {
        (0, 0, 0) => format!("{sec}s"),
        (0, 0, _) => format!("{m}m {sec}s"),
        (0, _, _) => format!("{h}h {m}m"),
        _ => format!("{days}d {h}h {m}m"),
    }
}

/// `85 MB`, `1.2 GB`.
pub fn format_bytes(bytes: u64) -> String {
    let mb = bytes as f64 / (1024.0 * 1024.0);
    if mb >= 1024.0 {
        format!("{:.1} GB", mb / 1024.0)
    } else {
        format!("{mb:.0} MB")
    }
}

/// Reply to `/about`: version, live numbers, credits and links (ephemeral).
pub fn about(v: &About, i18n: &I18n) -> Message {
    let t = |key: &str| i18n.t(&format!("bot.about.{key}"), &[]);
    let stat = |key: &str, value: &str| format!("{}: **{value}**", t(key));
    let mut build = i18n.t("bot.about.version", &[("version", v.version)]);
    if let Some(c) = v.commit {
        build.push_str(&format!(" · `{c}`"));
    }
    let head = vec![format!(
        "### Lynx
{}
-# {build}",
        t("tagline")
    )];
    let mut children = Component::section_or_text(head, v.avatar);
    children.push(Component::separator());

    let ping = match v.ping {
        Some(d) => format!("{} ms", d.as_millis()),
        None => t("measuring"),
    };
    let ytdlp = match &v.ytdlp {
        YtdlpLine::Known(version) => escape(&truncate(version, 40)),
        YtdlpLine::Checking => t("checking"),
        YtdlpLine::Unavailable => t("unavailable"),
    };
    let mut last = Vec::new();
    if let Some(b) = v.memory_bytes {
        last.push(stat("memory", &format_bytes(b)));
    }
    last.push(format!("yt-dlp: **{ytdlp}**"));
    let lines = [
        [stat("uptime", &format_uptime(v.uptime)), stat("ping", &ping)].join(" · "),
        [
            stat("servers", &v.servers.to_string()),
            stat("bots", &v.bots.to_string()),
        ]
        .join(" · "),
        [
            stat("voice_channels", &v.voice_channels.to_string()),
            stat("playing", &v.playing.to_string()),
        ]
        .join(" · "),
        last.join(" · "),
    ];
    children.push(Component::text(lines.join(
        "
",
    )));
    children.push(Component::separator());
    let credits = [
        t("license"),
        i18n.t(
            "bot.about.developed_by",
            &[("name", &link("PixlGalaxy", DEVELOPER_URL))],
        ),
        i18n.t("bot.about.hosted_on", &[("host", &link("ItzGalaxy.com", HOST_URL))]),
    ];
    children.push(Component::text(format!("-# {}", credits.join(" · "))));
    let mut row = vec![
        Button::link(i18n.t("bot.button.source", &[]), SOURCE_URL),
        Button::link(i18n.t("bot.button.invite_main", &[]), v.invite_url),
    ];
    row.extend(info_links(v.support_url, v.website_url, i18n));
    children.push(Component::Row(row));
    container(Tone::Info, children).ephemeral(true)
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;
    use crate::discord::ui::{FLAG_COMPONENTS_V2, FLAG_EPHEMERAL, MAX_TEXT_TOTAL};

    fn i18n() -> I18n {
        I18n::load("en").unwrap()
    }

    #[test]
    fn lyrics_are_escaped_line_by_line_and_cut_on_whole_lines() {
        let body = "# Verse\n[free nitro](https://evil.example)\n<@123> -# hi\n\n- la la";
        assert_eq!(
            lyrics_text(body, 3200),
            r"\# Verse
\[free nitro\]\(https://evil.example\)
\<\@123\> -\# hi

\- la la"
        );
        let long = "word\n".repeat(2000);
        let cut = lyrics_text(&long, 3200);
        assert!(cut.chars().count() <= 3204 && cut.ends_with("\n..."), "{}", cut.len());
        assert!(cut.trim_end_matches("\n...").lines().all(|l| l == "word"));
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
            ai: false,
        }
    }

    fn types(v: &Value) -> Vec<u64> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|c| c["type"].as_u64().unwrap())
            .collect()
    }

    fn collect(v: &Value, key: &str, out: &mut Vec<String>) {
        match v {
            Value::Object(m) => {
                for (k, x) in m {
                    match (k == key, x) {
                        (true, Value::String(s)) => out.push(s.clone()),
                        _ => collect(x, key, out),
                    }
                }
            }
            Value::Array(a) => a.iter().for_each(|x| collect(x, key, out)),
            _ => {}
        }
    }

    fn custom_ids(v: &Value) -> Vec<String> {
        let mut out = Vec::new();
        collect(v, "custom_id", &mut out);
        out
    }

    fn all_text(v: &Value) -> String {
        let mut out = Vec::new();
        collect(v, "content", &mut out);
        collect(v, "label", &mut out);
        out.join("\n")
    }

    /// Text must be translated and emoji-free (box drawing is allowed for the progress bar).
    fn check_text(s: &str) {
        assert!(!s.contains("bot."), "missing translation in {s:?}");
        for c in s.chars() {
            let cp = c as u32;
            assert!(
                cp < 0x2190 || (0x2500..0x2600).contains(&cp),
                "emoji-like char {c:?} in {s:?}"
            );
        }
    }

    #[test]
    fn jam_header_and_notices() {
        let i = i18n();
        let a = artists();
        let line = JamLine {
            host_name: "Lounge *1*".into(),
            channels: 3,
            hosting: false,
        };
        let mut v = NowPlaying {
            track: track(&a),
            voice_channel_id: 77,
            node_name: None,
            jam: Some(&line),
            genres: &[],
            paused: false,
            ai_enabled: false,
            track_tag: "tag",
        };
        let text = all_text(&now_playing(&v, &i).to_json());
        assert!(
            text.contains(r"Now playing in <#77> · Jam with Lounge \*1\* · 3 channels"),
            "{text}"
        );
        let hosting = JamLine {
            hosting: true,
            ..line.clone()
        };
        v.jam = Some(&hosting);
        let text = all_text(&now_playing(&v, &i).to_json());
        assert!(text.contains("Now playing in <#77> · Hosting a jam · 3 channels"), "{text}");

        // Other channels are named (they may be in another server), this one is a mention.
        let n = all_text(
            &notice(
                &Notice::JamJoined {
                    channel: "Lounge",
                    count: 2,
                },
                5,
                &i,
            )
            .to_json(),
        );
        assert!(n.starts_with("**Lounge joined the jam**"), "{n}");
        let n = all_text(&notice(&Notice::JamEnded { restored: true }, 5, &i).to_json());
        assert!(n.contains("<#5> is back to its own queue."), "{n}");
        let n = all_text(&error_key("jam_host_only", &[("channel", "**Lounge**")], &i).to_json());
        assert!(n.contains("**Lounge**"), "{n}");
        check_text(&n);
    }

    #[test]
    fn now_playing_marks_ai_picks() {
        let a = artists();
        let mut t = track(&a);
        let card = |t: &TrackCard| {
            let v = NowPlaying {
                track: t.clone(),
                voice_channel_id: 77,
                node_name: None,
                jam: None,
                genres: &[],
                paused: false,
                ai_enabled: true,
                track_tag: "tag",
            };
            all_text(&now_playing(&v, &i18n()).to_json())
        };
        assert!(!card(&t).contains("Selected by AI"));
        t.ai = true;
        assert!(card(&t).contains("Requested by <@42> · Selected by AI"), "{}", card(&t));
    }

    #[test]
    fn now_playing_shape() {
        let a = artists();
        let genres = vec!["synthwave".to_owned(), "electronic".to_owned()];
        let mut v = NowPlaying {
            track: track(&a),
            voice_channel_id: 77,
            node_name: Some("Lynx 2"),
            jam: None,
            genres: &genres,
            paused: false,
            ai_enabled: true,
            track_tag: "tag",
        };
        let json = now_playing(&v, &i18n()).to_json();
        assert_eq!(json["flags"], FLAG_COMPONENTS_V2);
        let container = &json["components"][0];
        assert_eq!(container["type"], 17);
        assert_eq!(container["accent_color"], COLOR_INFO);
        // section (with thumbnail), separator, two button rows
        assert_eq!(types(&container["components"]), vec![9, 14, 1, 1]);
        assert_eq!(container["components"][0]["accessory"]["type"], 11);
        let section = all_text(&container["components"][0]);
        assert!(
            section.contains("Now playing in <#77> · played by Lynx 2"),
            "{section}"
        );
        assert!(section.contains("[Song](<https://youtu.be/x>)"));
        assert!(section.contains("3:07 · Requested by <@42>"));
        assert!(section.contains("`synthwave` `electronic`"));
        assert_eq!(
            custom_ids(&json),
            vec![
                "np:pause:77:tag",
                "np:stop:77:tag",
                "np:skip:77:tag",
                "np:queue:77:tag",
                "np:like:77:tag",
                "np:dislike:77:tag",
                "np:jam:77:tag",
                "np:panel:77:-"
            ]
        );
        let styles = |row: &Value| -> Vec<u64> {
            row.as_array()
                .unwrap()
                .iter()
                .map(|b| b["style"].as_u64().unwrap())
                .collect()
        };
        // Pause, Stop, Skip, Queue: all grey.
        assert_eq!(styles(&container["components"][2]["components"]), vec![2, 2, 2, 2]);
        // Like green, Dislike red, Invite to jam grey, Open panel blue.
        assert_eq!(styles(&container["components"][3]["components"]), vec![3, 4, 2, 1]);
        check_text(&all_text(&json));
        assert!(!json.to_string().contains("emoji"), "no icons before the emoji sync");

        // With synced ids every button gets its variant: colored on grey, white on colored.
        let ids = |name: &str| Some(name.len() as u64);
        let json = now_playing(&v, &i18n()).to_json_with(&ids);
        let mut names = Vec::new();
        collect(&json, "name", &mut names);
        assert_eq!(
            names,
            vec![
                "lynx_pause_sky",
                "lynx_stop_red",
                "lynx_skip_emerald",
                "lynx_queue_amber",
                "lynx_like_white",
                "lynx_dislike_white",
                "lynx_jam_violet",
                "lynx_panel_white"
            ]
        );
        let labels: Vec<&str> = json["components"][0]["components"][2]["components"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["label"].as_str().unwrap())
            .collect();
        assert_eq!(labels, vec!["Pause", "Stop", "Skip", "Queue"], "labels stay");

        v.ai_enabled = false;
        v.paused = true;
        v.node_name = None;
        v.genres = &[];
        let json = now_playing(&v, &i18n()).to_json();
        let ids = custom_ids(&json);
        assert_eq!(ids[0], "np:resume:77:tag");
        assert_eq!(ids.len(), 6, "no Like/Dislike without AI");
        let json = now_playing(&v, &i18n()).to_json_with(&|_| Some(1));
        let resume = &json["components"][0]["components"][2]["components"][0];
        assert_eq!(
            (resume["style"].as_u64(), resume["emoji"]["name"].as_str()),
            (Some(2), Some("lynx_play_sky"))
        );
        assert!(!all_text(&json).contains("played by"));
    }

    /// "style emoji" of every button in a message rendered with all emoji ids known.
    fn look(m: Message) -> Vec<String> {
        fn walk(v: &Value, out: &mut Vec<String>) {
            match v {
                Value::Object(m) if m.get("type").and_then(Value::as_u64) == Some(2) => {
                    out.push(format!(
                        "{} {}",
                        m["style"],
                        m.get("emoji").and_then(|e| e["name"].as_str()).unwrap_or("-")
                    ))
                }
                Value::Object(m) => m.values().for_each(|x| walk(x, out)),
                Value::Array(a) => a.iter().for_each(|x| walk(x, out)),
                _ => {}
            }
        }
        let mut out = Vec::new();
        walk(&m.to_json_with(&|_| Some(9)), &mut out);
        out
    }

    #[test]
    fn button_styles_and_icons_per_card() {
        let i = i18n();
        let a = artists();
        let detail = NowPlayingDetail {
            track: track(&a),
            voice_channel_id: 1,
            node_name: None,
            jam: None,
            position_ms: 0,
            paused: false,
            loop_mode: LoopMode::Off,
            volume: 100,
            track_tag: "t",
        };
        assert_eq!(
            look(now_playing_detail(&detail, &i)),
            vec![
                "2 lynx_pause_sky",
                "2 lynx_skip_emerald",
                "2 lynx_queue_amber",
                "1 lynx_panel_white"
            ]
        );
        let q = QueuePage {
            voice_channel_id: 9,
            current: None,
            entries: vec![],
            page: 1,
            total_upcoming: 25,
            remaining_ms: 0,
            loop_mode: LoopMode::Off,
            issued: 1,
            expired: false,
        };
        assert_eq!(
            look(queue_page(&q, &i)),
            vec!["2 lynx_prev_slate", "2 lynx_next_slate", "1 lynx_panel_white"]
        );
        assert_eq!(
            look(notice(&Notice::QueueDeleted { minutes: 3 }, 5, &i)),
            vec!["2 lynx_replay_sky"]
        );
        assert_eq!(
            look(notice(&Notice::QueueFinished { ai_enabled: true }, 5, &i)),
            vec!["1 lynx_radio_white"]
        );
        assert_eq!(
            look(vote("T", "https://x", 1, 3, 7, "tg", &i)),
            vec!["2 lynx_skip_emerald"]
        );
        assert_eq!(look(radio_on(3, 7, &i)), vec!["2 lynx_radio_red", "1 lynx_panel_white"]);
        let picking = RadioPicking {
            voice_channel_id: 7,
            prompt: None,
            first_playing: false,
            steps: &[],
            job: "j",
        };
        assert_eq!(
            look(radio_picking(&picking, &i)),
            vec!["4 lynx_cancel_white", "1 lynx_panel_white"]
        );
        // Link buttons carry no icon of ours: Discord draws its own arrow.
        assert_eq!(look(jam(7, "https://discord.com/activities/1?custom_id=jam-x", Some("c"), &i)), vec!["5 -"]);
        assert_eq!(look(add_main("https://d", &i)), vec!["5 -"]);
        let added = AddedList {
            name: None,
            url: None,
            origin: TrackOrigin::Other,
            count: 1,
            total_ms: 0,
            thumbnail: None,
            preview: vec![],
            truncated: false,
            voice_channel_id: 3,
        };
        assert_eq!(
            look(added_list(&added, &i)),
            vec!["2 lynx_queue_amber", "1 lynx_panel_white"]
        );
    }

    #[test]
    fn now_playing_detail_has_progress() {
        let a = artists();
        let v = NowPlayingDetail {
            track: track(&a),
            voice_channel_id: 1,
            node_name: None,
            jam: None,
            position_ms: 67_000,
            paused: false,
            loop_mode: LoopMode::Off,
            volume: 100,
            track_tag: "t",
        };
        let json = now_playing_detail(&v, &i18n()).to_json();
        let text = all_text(&json);
        assert!(text.contains("`1:07` \u{2501}"), "{text}");
        assert!(text.contains("`3:07`"));
        assert!(text.contains("Loop: off") && text.contains("Volume: 100%"), "{text}");
        check_text(&text);
        assert_eq!(types(&json["components"][0]["components"]), vec![9, 14, 1]);
        assert_eq!(progress_bar(0, None), "`0:00`");
        let full = progress_bar(10_000, Some(10_000));
        assert!(full.ends_with("\u{25CF} `0:10`"), "{full}");
    }

    #[test]
    fn queue_page_shape() {
        let a = artists();
        let entries: Vec<(usize, TrackCard)> = (11..=20).map(|p| (p, track(&a))).collect();
        let v = QueuePage {
            voice_channel_id: 9,
            current: Some((track(&a), 67_000)),
            entries,
            page: 1,
            total_upcoming: 25,
            remaining_ms: 25 * 187_000,
            loop_mode: LoopMode::Off,
            issued: 1000,
            expired: false,
        };
        let json = queue_page(&v, &i18n()).to_json();
        let c = &json["components"][0]["components"];
        assert_eq!(types(c), vec![9, 14, 10, 14, 10, 1]);
        let head = all_text(&c[0]);
        assert!(
            head.contains("Queue in <#9>") && head.contains("25 songs") && head.contains("1:07 / 3:07"),
            "{head}"
        );
        assert!(c[2]["content"]
            .as_str()
            .unwrap()
            .starts_with("`11.` [Song](<https://youtu.be/x>) · Artist `3:07`"));
        assert!(c[4]["content"].as_str().unwrap().contains("Page 2 of 3"));
        assert_eq!(custom_ids(&json), vec!["q:9:0:1000", "q:9:2:1000", "np:panel:9:-"]);
        assert!(queue_page(&v, &i18n()).text_len() <= MAX_TEXT_TOTAL);
        check_text(&all_text(&json));

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
        let expired = QueuePage {
            expired: true,
            ..v.clone()
        };
        assert_eq!(
            custom_ids(&queue_page(&expired, &i18n()).to_json()),
            vec!["np:panel:9:-"]
        );
        let single = QueuePage {
            page: 0,
            total_upcoming: 3,
            current: None,
            ..v
        };
        let json = queue_page(&single, &i18n()).to_json();
        assert_eq!(custom_ids(&json), vec!["np:panel:9:-"]);
        assert_eq!(
            types(&json["components"][0]["components"])[0],
            10,
            "no thumbnail without a current track"
        );
        assert_eq!(page_count(0), 1);
        assert_eq!(page_count(10), 1);
        assert_eq!(page_count(11), 2);
    }

    #[test]
    fn added_shapes() {
        let i = i18n();
        let a = artists();
        let json = added_single(&track(&a), 4, Some(9 * 60_000 + 5_000), &i).to_json();
        assert_eq!(json["components"][0]["accent_color"], COLOR_INFO);
        assert_eq!(types(&json["components"][0]["components"]), vec![9]);
        let text = all_text(&json);
        assert!(
            text.contains("Position 4 in the queue · Plays in about 9 minutes"),
            "{text}"
        );
        assert!(all_text(&added_single(&track(&a), 0, None, &i).to_json()).contains("Starting in a few seconds"));

        let preview: Vec<(usize, TrackCard)> = (5..10).map(|p| (p, track(&a))).collect();
        let list = AddedList {
            name: Some("Late Night Mix"),
            url: Some("https://open.spotify.com/playlist/x"),
            origin: TrackOrigin::Spotify,
            count: 24,
            total_ms: 5_050_000,
            thumbnail: Some("https://i/x.jpg"),
            preview,
            truncated: false,
            voice_channel_id: 3,
        };
        let json = added_list(&list, &i).to_json();
        let text = all_text(&json);
        assert!(text.contains("24 songs from [Late Night Mix]"), "{text}");
        assert!(text.contains("Spotify playlist · 1:24:10 in total"), "{text}");
        assert!(text.contains("and 19 more"), "{text}");
        assert_eq!(types(&json["components"][0]["components"]), vec![9, 14, 10, 1]);
        assert_eq!(custom_ids(&json), vec!["np:queue:3:-", "np:panel:3:-"]);
        check_text(&text);
    }

    #[test]
    fn notices_and_replies() {
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

        let json = notice(
            &Notice::JamEmpty {
                deadline_secs: 1_700_000_180,
            },
            5,
            &i,
        )
        .to_json();
        let text = all_text(&json);
        assert!(text.contains("<t:1700000180:R>"), "{text}");
        assert!(text.contains("leave the jam and disconnect"), "{text}");
        assert_eq!(json["components"][0]["accent_color"], COLOR_WARNING);
        check_text(&text);

        let json = notice(&Notice::QueueDeleted { minutes: 3 }, 5, &i).to_json();
        assert_eq!(json["components"][0]["accent_color"], COLOR_ERROR);
        assert_eq!(json["components"][0]["components"][0]["accessory"]["custom_id"], "re:5");
        assert!(all_text(&json).contains("in 3 minutes"));

        let with_ai = notice(&Notice::QueueFinished { ai_enabled: true }, 5, &i).to_json();
        assert_eq!(custom_ids(&with_ai), vec!["ai:5"]);
        let without = notice(&Notice::QueueFinished { ai_enabled: false }, 5, &i).to_json();
        assert!(custom_ids(&without).is_empty());

        for n in [
            Notice::Resumed {
                title: "T",
                url: "https://x",
            },
            Notice::BackOnline {
                title: "T",
                url: "https://x",
                position: 4,
                total: 10,
            },
            Notice::TrackFailed {
                title: "T",
                url: "https://x",
                reason: "gone",
            },
            Notice::TooManyFailures,
            Notice::Disconnected,
            Notice::Stopped { by: 1 },
            Notice::JamEntered { host: "General" },
            Notice::JamJoined {
                channel: "Lounge",
                count: 3,
            },
            Notice::JamLeft {
                channel: "Lounge",
                count: 2,
            },
            Notice::JamHost {
                channel: "Lounge",
                own: false,
            },
            Notice::JamHost {
                channel: "Lounge",
                own: true,
            },
            Notice::JamEnded { restored: true },
            Notice::JamEnded { restored: false },
            Notice::JamRemoved,
            Notice::JamLeftOwn { restored: true },
            Notice::JamBack,
        ] {
            let json = notice(&n, 5, &i).to_json();
            assert_eq!(json["flags"], FLAG_COMPONENTS_V2, "notices are public");
            check_text(&all_text(&json));
        }

        let e = error_key("not_in_voice", &[], &i).to_json();
        assert_eq!(e["flags"], FLAG_COMPONENTS_V2 | FLAG_EPHEMERAL);
        assert_eq!(e["components"][0]["accent_color"], COLOR_ERROR);
        assert!(all_text(&e).starts_with("**Join a voice channel first.**"));

        let busy = all_busy(Some(1), 2, Some((1, "https://discord.com/x")), &i).to_json();
        assert_eq!(busy["components"][0]["accent_color"], COLOR_WARNING);
        let acc = &busy["components"][0]["components"][0]["accessory"];
        assert_eq!(
            (acc["style"].as_u64(), acc["label"].as_str()),
            (Some(5), Some("Add another Lynx"))
        );
        check_text(&all_text(&all_busy(None, 2, None, &i).to_json()));

        let share = "https://discord.com/activities/42?custom_id=jam-abcdefghij";
        let j = jam(7, share, Some("aB3"), &i).to_json();
        assert_eq!(j["flags"], FLAG_COMPONENTS_V2 | FLAG_EPHEMERAL);
        assert_eq!(j["components"][0]["accent_color"], COLOR_SUCCESS);
        assert_eq!(j["components"][0]["components"][0]["accessory"]["url"], share);
        // Links show as plain URLs: a masked link labelled like a URL is printed raw.
        let jt = all_text(&j);
        assert!(jt.contains(share), "{jt}");
        assert!(jt.contains("https://discord.gg/aB3"), "{jt}");
        assert!(!jt.contains("](<"), "{jt}");
        check_text(&jt);
        // Without an invite (no Create Invite permission) only the jam link is there.
        let jt = all_text(&jam(7, share, None, &i).to_json());
        assert!(!jt.contains("discord.gg"), "{jt}");

        let v = vote("T", "https://x", 2, 3, 7, "tg", &i).to_json();
        assert!(all_text(&v).contains("Vote to skip: 2 of 3"));
        assert!(all_text(&v).contains("one more listener"));
        assert_eq!(custom_ids(&v), vec!["np:skip:7:tg"]);

        let h = history_off(7, &i).to_json();
        assert_eq!(h["components"][0]["accent_color"], COLOR_WARNING);
        assert_eq!(h["flags"], FLAG_COMPONENTS_V2 | FLAG_EPHEMERAL);
        let m = add_main("https://discord.com/y", &i).to_json();
        assert_eq!(
            m["components"][0]["components"][1]["components"][0]["url"],
            "https://discord.com/y"
        );
        assert_eq!(node_notice("https://d", &i).to_json()["flags"], FLAG_COMPONENTS_V2);

        let a = artists();
        let l = lyrics(&track(&a), &"line\n".repeat(2000), Some(3), &i);
        assert!(l.text_len() <= MAX_TEXT_TOTAL);
        assert_eq!(
            types(&l.to_json()["components"][0]["components"]),
            vec![9, 14, 10, 14, 10, 1]
        );
    }

    fn about_info(ping: Option<std::time::Duration>, memory: Option<u64>, ytdlp: YtdlpLine) -> About<'static> {
        About {
            version: "0.1.0",
            commit: Some("abc1234"),
            avatar: Some("https://cdn.discordapp.com/avatars/1/a.png"),
            uptime: std::time::Duration::from_secs(3 * 86_400 + 4 * 3600 + 12 * 60 + 9),
            ping,
            servers: 120,
            bots: 3,
            voice_channels: 5,
            playing: 2,
            memory_bytes: memory,
            ytdlp,
            invite_url: "https://discord.com/oauth2/authorize?client_id=1",
            support_url: Some("https://discord.gg/support"),
            website_url: Some("https://lynx.example"),
        }
    }

    fn link_urls(json: &Value) -> Vec<String> {
        let mut out = Vec::new();
        collect(json, "url", &mut out);
        out.retain(|u| !u.contains("cdn.discordapp.com"));
        out
    }

    #[test]
    fn invite_and_about_cards() {
        for lang in ["en", "es"] {
            let i = I18n::load(lang).unwrap();
            let inv = invite(
                "https://discord.com/x",
                Some("https://discord.gg/s"),
                Some("https://w"),
                &i,
            )
            .to_json();
            assert_eq!(inv["flags"], FLAG_COMPONENTS_V2 | FLAG_EPHEMERAL);
            assert_eq!(types(&inv["components"][0]["components"]), vec![10, 1]);
            assert_eq!(
                link_urls(&inv),
                vec!["https://discord.com/x", "https://discord.gg/s", "https://w"]
            );
            check_text(&all_text(&inv));

            for v in [
                about_info(
                    Some(std::time::Duration::from_millis(42)),
                    Some(85 * 1024 * 1024),
                    YtdlpLine::Known("2025.10.22".into()),
                ),
                about_info(None, None, YtdlpLine::Checking),
                about_info(None, None, YtdlpLine::Unavailable),
            ] {
                let json = about(&v, &i).to_json();
                assert_eq!(json["flags"], FLAG_COMPONENTS_V2 | FLAG_EPHEMERAL);
                let text = all_text(&json);
                check_text(&text);
                assert!(!text.to_lowercase().contains("node"), "{text}");
                assert!(about(&v, &i).text_len() <= MAX_TEXT_TOTAL);
            }
        }
        // No support or website configured: only the invite (and source code) buttons.
        let i = i18n();
        let inv = invite("https://discord.com/x", None, Some("not a url"), &i).to_json();
        assert_eq!(link_urls(&inv), vec!["https://discord.com/x"]);
        assert_eq!(look(invite("https://d", None, None, &i)), vec!["5 -"]);

        let v = about_info(
            Some(std::time::Duration::from_millis(42)),
            Some(85 * 1024 * 1024),
            YtdlpLine::Known("2025.10.22".into()),
        );
        let json = about(&v, &i).to_json();
        let c = &json["components"][0]["components"];
        // section with the avatar, separator, stats, separator, credits, links
        assert_eq!(types(c), vec![9, 14, 10, 14, 10, 1]);
        let text = all_text(&json);
        assert!(
            text.contains("### Lynx") && text.contains("-# Version 0.1.0 · `abc1234`"),
            "{text}"
        );
        assert!(
            text.contains("Uptime: **3d 4h 12m** · Gateway ping: **42 ms**"),
            "{text}"
        );
        assert!(text.contains("Servers: **120** · Lynx bots: **3**"), "{text}");
        assert!(text.contains("Voice channels: **5** · Playing now: **2**"), "{text}");
        assert!(text.contains("Memory: **85 MB** · yt-dlp: **2025.10.22**"), "{text}");
        assert!(text.contains("AGPL-3.0 with Commons Clause · Developed by [PixlGalaxy](<https://github.com/PixlGalaxy>) · Hosted on [ItzGalaxy.com](<https://itzgalaxy.com>)"), "{text}");
        assert_eq!(
            link_urls(&json),
            vec![
                SOURCE_URL,
                "https://discord.com/oauth2/authorize?client_id=1",
                "https://discord.gg/support",
                "https://lynx.example"
            ]
        );
        // Link buttons carry no icon of ours.
        assert_eq!(look(about(&v, &i)), vec!["5 -", "5 -", "5 -", "5 -"]);

        let early = about(&about_info(None, None, YtdlpLine::Checking), &i).to_json();
        let text = all_text(&early);
        assert!(text.contains("Gateway ping: **measuring**"), "{text}");
        assert!(
            text.contains(
                "
yt-dlp: **checking**"
            ) && !text.contains("Memory"),
            "{text}"
        );
        let es = all_text(
            &about(
                &about_info(None, None, YtdlpLine::Unavailable),
                &I18n::load("es").unwrap(),
            )
            .to_json(),
        );
        assert!(
            es.contains("Versión 0.1.0") && es.contains("yt-dlp: **no disponible**"),
            "{es}"
        );
    }

    #[test]
    fn uptime_and_memory_format() {
        use std::time::Duration;
        assert_eq!(format_uptime(Duration::from_secs(5)), "5s");
        assert_eq!(format_uptime(Duration::from_secs(12 * 60 + 5)), "12m 5s");
        assert_eq!(format_uptime(Duration::from_secs(4 * 3600 + 12 * 60 + 5)), "4h 12m");
        assert_eq!(format_uptime(Duration::from_secs(86_400 + 5)), "1d 0h 0m");
        assert_eq!(format_bytes(85 * 1024 * 1024 + 300_000), "85 MB");
        assert_eq!(format_bytes(1229 * 1024 * 1024), "1.2 GB");
    }
}
