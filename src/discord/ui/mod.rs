//! Small typed builder for Discord Components V2 messages (serenity 0.12 has no support for them).
//!
//! A message built here is sent as raw JSON through serenity's HTTP client. With the
//! `IS_COMPONENTS_V2` flag, `content` and `embeds` are not allowed and the flag cannot be removed
//! by later edits.

pub mod cards;

use serde_json::{json, Value};

use crate::text::sanitize_text;

pub use super::emojis::Icon;

/// Looks up the id of an application emoji by name (`emojis::emoji_id` outside tests).
pub type EmojiIds<'a> = &'a dyn Fn(&str) -> Option<u64>;

pub const FLAG_EPHEMERAL: u64 = 1 << 6;
pub const FLAG_COMPONENTS_V2: u64 = 1 << 15;

/// Total text Discord accepts across all text displays of one message.
pub const MAX_TEXT_TOTAL: usize = 4000;
/// Components (nested included) Discord accepts in one Components V2 message.
pub const MAX_COMPONENTS: usize = 40;

pub const COLOR_INFO: u32 = 0x0EA5E9;
pub const COLOR_SUCCESS: u32 = 0x23A55A;
pub const COLOR_WARNING: u32 = 0xF0B232;
pub const COLOR_ERROR: u32 = 0xDA373C;

/// Interaction callback types used by the bot.
pub mod callback {
    pub const MESSAGE: u8 = 4;
    pub const DEFERRED_MESSAGE: u8 = 5;
    pub const UPDATE_MESSAGE: u8 = 7;
    pub const AUTOCOMPLETE: u8 = 8;
    pub const LAUNCH_ACTIVITY: u8 = 12;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonStyle {
    Primary = 1,
    Secondary = 2,
    Success = 3,
    Danger = 4,
    Link = 5,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Button {
    pub style: ButtonStyle,
    pub label: String,
    /// `custom_id` for normal buttons, URL for link buttons.
    pub target: String,
    pub disabled: bool,
    /// Shown as an application emoji next to the label once its id is known.
    pub icon: Option<Icon>,
}

impl Button {
    pub fn new(style: ButtonStyle, label: impl Into<String>, custom_id: impl Into<String>) -> Self {
        Self {
            style,
            label: label.into(),
            target: custom_id.into(),
            disabled: false,
            icon: None,
        }
    }

    pub fn link(label: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            style: ButtonStyle::Link,
            label: label.into(),
            target: url.into(),
            disabled: false,
            icon: None,
        }
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub fn icon(mut self, icon: Icon) -> Self {
        self.icon = Some(icon);
        self
    }

    fn to_json(&self, emoji_ids: EmojiIds) -> Value {
        let mut v = json!({ "type": 2, "style": self.style as u8, "label": truncate(&self.label, 80) });
        let emoji = self
            .icon
            .and_then(|i| i.emoji_name(self.style))
            .and_then(|name| emoji_ids(name).map(|id| (name, id)));
        if let Some((name, id)) = emoji {
            v["emoji"] = json!({ "id": id.to_string(), "name": name });
        }
        if self.style == ButtonStyle::Link {
            v["url"] = json!(self.target);
        } else {
            v["custom_id"] = json!(truncate(&self.target, 100));
        }
        if self.disabled {
            v["disabled"] = json!(true);
        }
        v
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Accessory {
    Thumbnail { url: String },
    Button(Button),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Component {
    Container {
        accent: Option<u32>,
        children: Vec<Component>,
    },
    /// 1-3 text blocks next to an accessory.
    Section {
        texts: Vec<String>,
        accessory: Accessory,
    },
    Text(String),
    Separator {
        divider: bool,
        large: bool,
    },
    Row(Vec<Button>),
}

impl Component {
    pub fn text(s: impl Into<String>) -> Self {
        Self::Text(s.into())
    }

    pub fn separator() -> Self {
        Self::Separator {
            divider: true,
            large: false,
        }
    }

    /// A section with a thumbnail when there is an image, plain text displays otherwise.
    pub fn section_or_text(mut texts: Vec<String>, thumbnail: Option<&str>) -> Vec<Self> {
        match thumbnail.filter(|u| u.starts_with("https://") || u.starts_with("http://")) {
            Some(url) => {
                // A section takes at most three text displays; merge the rest into the last one.
                while texts.len() > 3 {
                    let last = texts.pop().unwrap_or_default();
                    if let Some(prev) = texts.last_mut() {
                        prev.push('\n');
                        prev.push_str(&last);
                    }
                }
                vec![Self::Section {
                    texts,
                    accessory: Accessory::Thumbnail { url: url.to_owned() },
                }]
            }
            None => texts.into_iter().map(Self::Text).collect(),
        }
    }

    fn to_json(&self, emoji_ids: EmojiIds) -> Value {
        match self {
            Self::Container { accent, children } => {
                let children: Vec<Value> = children.iter().map(|c| c.to_json(emoji_ids)).collect();
                let mut v = json!({ "type": 17, "components": children });
                if let Some(c) = accent {
                    v["accent_color"] = json!(c);
                }
                v
            }
            Self::Section { texts, accessory } => json!({
                "type": 9,
                "components": texts.iter().map(|t| json!({ "type": 10, "content": t })).collect::<Vec<_>>(),
                "accessory": match accessory {
                    Accessory::Thumbnail { url } => json!({ "type": 11, "media": { "url": url } }),
                    Accessory::Button(b) => b.to_json(emoji_ids),
                },
            }),
            Self::Text(t) => json!({ "type": 10, "content": t }),
            Self::Separator { divider, large } => {
                json!({ "type": 14, "divider": divider, "spacing": if *large { 2 } else { 1 } })
            }
            Self::Row(buttons) => {
                json!({ "type": 1, "components": buttons.iter().take(5).map(|b| b.to_json(emoji_ids)).collect::<Vec<_>>() })
            }
        }
    }

    pub fn text_len(&self) -> usize {
        match self {
            Self::Container { children, .. } => children.iter().map(Self::text_len).sum(),
            Self::Section { texts, .. } => texts.iter().map(|t| t.chars().count()).sum(),
            Self::Text(t) => t.chars().count(),
            _ => 0,
        }
    }

    /// Components Discord counts toward its limit (nested ones included).
    pub fn count(&self) -> usize {
        match self {
            Self::Container { children, .. } => 1 + children.iter().map(Self::count).sum::<usize>(),
            Self::Section { texts, .. } => 2 + texts.len(),
            Self::Row(buttons) => 1 + buttons.len().min(5),
            Self::Text(_) | Self::Separator { .. } => 1,
        }
    }

    fn contains_text(&self, needle: &str) -> bool {
        match self {
            Self::Container { children, .. } => children.iter().any(|c| c.contains_text(needle)),
            Self::Section { texts, .. } => texts.iter().any(|t| t.contains(needle)),
            Self::Text(t) => t.contains(needle),
            _ => false,
        }
    }
}

/// A complete Components V2 message.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub components: Vec<Component>,
    pub ephemeral: bool,
}

impl Message {
    pub fn new(components: Vec<Component>) -> Self {
        Self {
            components,
            ephemeral: false,
        }
    }

    pub fn ephemeral(mut self, yes: bool) -> Self {
        self.ephemeral = yes;
        self
    }

    pub fn flags(&self) -> u64 {
        FLAG_COMPONENTS_V2 | if self.ephemeral { FLAG_EPHEMERAL } else { 0 }
    }

    pub fn text_len(&self) -> usize {
        self.components.iter().map(Component::text_len).sum()
    }

    /// Body for `POST /channels/{id}/messages`, message edits and followups.
    /// Mentions never ping anyone (requester lines use `<@id>`).
    /// Buttons get their icons from the application emojis synced at startup.
    /// While an operator notice is visible (scheduled downtime...), its line is added at the end.
    pub fn to_json(&self) -> Value {
        match crate::notices::current_discord_line() {
            Some(line) => self.with_notice(&line).to_json_with(&super::emojis::emoji_id),
            None => self.to_json_with(&super::emojis::emoji_id),
        }
    }

    /// This message plus the notice line as a last text display, unless it already shows that
    /// line or adding it would break Discord's limits (text total, 40 components).
    pub fn with_notice(&self, line: &str) -> Message {
        let mut m = self.clone();
        let fits = self.text_len() + line.chars().count() <= MAX_TEXT_TOTAL
            && self.components.iter().map(Component::count).sum::<usize>() < MAX_COMPONENTS;
        if fits && !self.components.iter().any(|c| c.contains_text(line)) {
            m.components.push(Component::Text(line.to_owned()));
        }
        m
    }

    /// `to_json` with an explicit emoji id lookup.
    pub fn to_json_with(&self, emoji_ids: EmojiIds) -> Value {
        debug_assert!(self.text_len() <= MAX_TEXT_TOTAL, "message text over the Discord limit");
        json!({
            "flags": self.flags(),
            "components": self.components.iter().map(|c| c.to_json(emoji_ids)).collect::<Vec<_>>(),
            "allowed_mentions": { "parse": [] },
        })
    }

    /// Body for an interaction callback (`type` 4 = new message, 7 = update the clicked message).
    pub fn to_callback(&self, kind: u8) -> Value {
        json!({ "type": kind, "data": self.to_json() })
    }
}

/// Callback that only carries a type (LAUNCH_ACTIVITY).
pub fn bare_callback(kind: u8) -> Value {
    json!({ "type": kind })
}

/// Deferred "thinking" response; ephemeral or public.
pub fn defer_callback(ephemeral: bool) -> Value {
    json!({ "type": callback::DEFERRED_MESSAGE, "data": { "flags": if ephemeral { FLAG_EPHEMERAL } else { 0 } } })
}

/// Autocomplete choices (name and value are capped at 100 characters each, at most 25).
pub fn autocomplete_callback(choices: &[(String, String)]) -> Value {
    let list: Vec<Value> = choices
        .iter()
        .filter(|(_, value)| !value.is_empty() && value.chars().count() <= 100)
        .take(25)
        .map(|(name, value)| json!({ "name": truncate(name, 100), "value": value }))
        .collect();
    json!({ "type": callback::AUTOCOMPLETE, "data": { "choices": list } })
}

/// Cut `s` to at most `max` characters, adding an ellipsis when something was removed.
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max.saturating_sub(3)).collect();
    out.push_str("...");
    out
}

/// Escape Discord markdown in user-provided text (titles, names).
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(
            c,
            '\\' | '*' | '_' | '~' | '`' | '|' | '[' | ']' | '>' | '#' | '<' | '(' | ')' | '@'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// One line of multi-line text (lyrics) as plain text: [`sanitize_text`], [`escape`], and line-start
/// markdown (`- ` / `+ ` lists, `1. ` numbered lists, `-#` subtext) neutralised; `#` headings and `>`
/// quotes are already escaped by [`escape`].
pub fn escape_line(line: &str) -> String {
    let mut out = escape(&sanitize_text(line));
    if out.starts_with('-') || out.starts_with('+') {
        out.insert(0, '\\');
    } else if let Some(dot) = out.find('.').filter(|i| *i > 0 && out[..*i].bytes().all(|b| b.is_ascii_digit())) {
        out.insert(dot, '\\');
    }
    out
}

/// Make text safe inside a masked link label `[label](url)` without backslashes: Discord shows
/// backslash escapes literally there. Brackets become parentheses (parentheses are fine inside a
/// label), markdown characters become look-alikes, and `@` gets a zero-width space so `@everyone`
/// never reads as a mention. Angle brackets become look-alikes (no `<@id>` / `<#id>` / `<t:..>`
/// tokens) and newlines / control characters spaces (one line, see [`sanitize_text`]).
pub fn link_label(s: &str) -> String {
    let s = sanitize_text(s);
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '[' => out.push('('),
            ']' => out.push(')'),
            '<' => out.push('\u{2039}'), // single left-pointing angle quotation mark
            '>' => out.push('\u{203A}'), // single right-pointing angle quotation mark
            '\\' => out.push('\u{29F5}'), // reverse solidus operator
            '*' => out.push('\u{2217}'),  // asterisk operator
            '_' => out.push('\u{02CD}'),  // modifier letter low macron
            '~' => out.push('\u{223C}'),  // tilde operator
            '`' => out.push('\u{02CB}'),  // modifier letter grave accent
            '|' => out.push('\u{01C0}'),  // latin letter dental click
            '@' => out.push_str("@\u{200B}"),
            c => out.push(c),
        }
    }
    out
}

/// `[title](<url>)` with a safe label; plain escaped title when the URL is not http(s).
pub fn link(title: &str, url: &str) -> String {
    let title = truncate(&sanitize_text(title), 200);
    if url.starts_with("https://") || url.starts_with("http://") {
        format!("[{}](<{}>)", link_label(&title), link_target(url))
    } else {
        escape(&title)
    }
}

/// A URL that can't end the `(<...>)` target early or break the line.
fn link_target(url: &str) -> String {
    let mut out = String::with_capacity(url.len());
    for c in url.chars() {
        match c {
            '>' => out.push_str("%3E"),
            '<' => out.push_str("%3C"),
            ')' => out.push_str("%29"),
            c if c.is_whitespace() => out.push_str("%20"),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// `3:07`, `1:02:03`.
pub fn format_duration(ms: u64) -> String {
    let secs = ms / 1000;
    let (h, m, s) = (secs / 3600, (secs / 60) % 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Discord relative timestamp (`<t:1700000000:R>`), rendered in each viewer's own time zone.
pub fn relative_time(epoch_secs: i64) -> String {
    format!("<t:{epoch_secs}:R>")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_json_shape() {
        let msg = Message::new(vec![Component::Container {
            accent: Some(COLOR_INFO),
            children: vec![
                Component::Section {
                    texts: vec!["a".into(), "b".into()],
                    accessory: Accessory::Thumbnail {
                        url: "https://img/x.jpg".into(),
                    },
                },
                Component::separator(),
                Component::Row(vec![
                    Button::new(ButtonStyle::Primary, "Pause", "np:pause"),
                    Button::link("Site", "https://x"),
                ]),
            ],
        }]);
        let v = msg.to_json();
        assert_eq!(
            v,
            json!({
                "flags": 32768,
                "allowed_mentions": { "parse": [] },
                "components": [{
                    "type": 17,
                    "accent_color": 0x0EA5E9,
                    "components": [
                        {
                            "type": 9,
                            "components": [{ "type": 10, "content": "a" }, { "type": 10, "content": "b" }],
                            "accessory": { "type": 11, "media": { "url": "https://img/x.jpg" } }
                        },
                        { "type": 14, "divider": true, "spacing": 1 },
                        {
                            "type": 1,
                            "components": [
                                { "type": 2, "style": 1, "label": "Pause", "custom_id": "np:pause" },
                                { "type": 2, "style": 5, "label": "Site", "url": "https://x" }
                            ]
                        }
                    ]
                }]
            })
        );
        assert_eq!(Message::new(vec![]).ephemeral(true).flags(), 32768 | 64);
        assert_eq!(msg.to_callback(callback::UPDATE_MESSAGE)["type"], 7);
    }

    #[test]
    fn buttons_carry_emojis_only_with_known_ids() {
        let msg = Message::new(vec![Component::Row(vec![
            Button::new(ButtonStyle::Secondary, "Skip", "np:skip").icon(Icon::Skip),
            Button::new(ButtonStyle::Primary, "Open panel", "np:panel").icon(Icon::Panel),
            Button::new(ButtonStyle::Secondary, "Plain", "x"),
            Button::link("Site", "https://x"),
        ])]);
        let ids = |name: &str| match name {
            "lynx_skip_emerald" => Some(11),
            "lynx_panel_white" => Some(22),
            _ => None,
        };
        let row = &msg.to_json_with(&ids)["components"][0]["components"];
        assert_eq!(
            row[0],
            json!({ "type": 2, "style": 2, "label": "Skip", "custom_id": "np:skip",
                    "emoji": { "id": "11", "name": "lynx_skip_emerald" } })
        );
        assert_eq!(row[1]["emoji"], json!({ "id": "22", "name": "lynx_panel_white" }));
        assert!(row[2].get("emoji").is_none(), "no icon, no emoji");
        assert!(row[3].get("emoji").is_none(), "link buttons keep only Discord's own arrow");

        // Without ids (sync not done or failed) the buttons are text-only, exactly as before.
        let none = msg.to_json_with(&|_| None);
        let row = &none["components"][0]["components"];
        assert_eq!(
            row[0],
            json!({ "type": 2, "style": 2, "label": "Skip", "custom_id": "np:skip" })
        );
        assert!(row.as_array().unwrap().iter().all(|b| b.get("emoji").is_none()));
    }

    #[test]
    fn section_falls_back_to_text_without_image() {
        let c = Component::section_or_text(vec!["a".into(), "b".into()], None);
        assert_eq!(c, vec![Component::text("a"), Component::text("b")]);
        let c = Component::section_or_text(vec!["1".into(), "2".into(), "3".into(), "4".into()], Some("https://i"));
        match &c[0] {
            Component::Section { texts, .. } => assert_eq!(texts, &vec!["1".to_owned(), "2".into(), "3\n4".into()]),
            other => panic!("{other:?}"),
        }
        // Non-http thumbnails are ignored.
        assert_eq!(Component::section_or_text(vec!["a".into()], Some("data:x")).len(), 1);
    }

    #[test]
    fn text_helpers() {
        assert_eq!(format_duration(187_000), "3:07");
        assert_eq!(format_duration(3_723_000), "1:02:03");
        assert_eq!(escape("a*b_[c]"), "a\\*b\\_\\[c\\]");
        assert_eq!(link("T", "https://y/z"), "[T](<https://y/z>)");
        assert_eq!(link("T", "ftp://y"), "T");
        assert_eq!(link("a*b", "ftp://y"), "a\\*b", "plain text keeps backslash escapes");
        assert_eq!(truncate("abcdef", 5), "ab...");
        assert_eq!(truncate("abc", 5), "abc");
        assert_eq!(relative_time(5), "<t:5:R>");
        let ac = autocomplete_callback(&[("n".into(), "v".into()), ("n2".into(), "x".repeat(101))]);
        assert_eq!(ac["data"]["choices"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn link_labels_never_contain_backslashes() {
        let url = "https://youtu.be/x";
        assert_eq!(
            link("Taking over (Synchronice Remix) [feat. Cassandra Kay]", url),
            "[Taking over (Synchronice Remix) (feat. Cassandra Kay)](<https://youtu.be/x>)"
        );
        assert_eq!(
            link("Live]Set", url),
            "[Live)Set](<https://youtu.be/x>)",
            "] cannot end the label"
        );
        assert_eq!(link("[Intro]", url), "[(Intro)](<https://youtu.be/x>)");
        assert_eq!(
            link("Hello *World*", url),
            "[Hello \u{2217}World\u{2217}](<https://youtu.be/x>)"
        );
        assert_eq!(
            link("my_song_name", url),
            "[my\u{02CD}song\u{02CD}name](<https://youtu.be/x>)"
        );
        assert_eq!(link("a ~~b~~ `c` ||d||", url), "[a \u{223C}\u{223C}b\u{223C}\u{223C} \u{02CB}c\u{02CB} \u{01C0}\u{01C0}d\u{01C0}\u{01C0}](<https://youtu.be/x>)");
        assert_eq!(
            link("@everyone hi", url),
            "[@\u{200B}everyone hi](<https://youtu.be/x>)"
        );
        assert_eq!(link("AC\\DC", url), "[AC\u{29F5}DC](<https://youtu.be/x>)");
        for title in [
            "Taking over (Synchronice Remix) [feat. Cassandra Kay]",
            "a]b",
            "*_~`|>\\[]()",
        ] {
            let out = link(title, url);
            assert!(!out.contains('\\'), "{out}");
            let label = &out[1..out.rfind("](<").unwrap()];
            assert!(!label.contains('[') && !label.contains(']'), "{out}");
        }
    }

    #[test]
    fn labels_are_one_line_without_discord_tokens() {
        let url = "https://youtu.be/x";
        let out = link("Hi <@123> <#456> <t:1:R>\n# Big\r\nline", url);
        assert!(!out.contains('\n') && !out.contains('\r'), "{out}");
        let label = &out[1..out.rfind("](<").unwrap()];
        assert_eq!(
            label,
            "Hi \u{2039}@\u{200B}123\u{203A} \u{2039}#456\u{203A} \u{2039}t:1:R\u{203A} # Big line"
        );
        // The target can't be closed early or broken over lines.
        assert_eq!(
            link("T", "https://e.com/a>b) c\nd"),
            "[T](<https://e.com/a%3Eb%29%20c%20d>)"
        );
        // Plain text (no link) is one line too.
        assert_eq!(link("a\nb <@1>", "ftp://y"), r"a b \<\@1\>");
    }

    #[test]
    fn lyric_lines_are_plain_text() {
        assert_eq!(escape_line("# Chorus"), r"\# Chorus");
        assert_eq!(escape_line("-# small"), r"\-\# small");
        assert_eq!(escape_line("- item"), r"\- item");
        assert_eq!(escape_line("+ item"), r"\+ item");
        assert_eq!(escape_line("> quote"), r"\> quote");
        assert_eq!(escape_line("* bullet"), r"\* bullet");
        assert_eq!(escape_line("1. first"), r"1\. first");
        assert_eq!(
            escape_line("[click](https://evil.example)"),
            r"\[click\]\(https://evil.example\)"
        );
        assert_eq!(escape_line("<@&42> @everyone"), r"\<\@&42\> \@everyone");
        assert_eq!(escape_line("one - two"), "one - two");
        assert_eq!(escape_line("3.14 is pi"), r"3\.14 is pi");
        assert_eq!(escape_line(""), "");
    }
}

#[cfg(test)]
mod notice_tests {
    use std::sync::Arc;

    use super::*;
    use crate::{
        i18n::I18n,
        notices::{self, test_util::notice, Kind, NoticeBoard},
        store::now_secs,
    };

    fn texts(v: &Value) -> Vec<String> {
        v["components"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|c| c["content"].as_str().map(String::from))
            .collect()
    }

    #[test]
    fn every_message_carries_the_visible_notice() {
        notices::uninstall();
        let msg = Message::new(vec![Component::Container {
            accent: None,
            children: vec![Component::text("Paused.")],
        }]);
        assert!(texts(&msg.to_json()).is_empty(), "no board, no line");

        let board = NoticeBoard::default();
        notices::install(board.clone(), Arc::new(I18n::load("en").unwrap()));
        let now = now_secs();
        board.set_notices(vec![notice(1, Kind::Maintenance, now + 3600, 10, now - 5)]);
        let v = msg.to_json();
        let lines = texts(&v);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].starts_with(&format!("-# Scheduled downtime on <t:{}:F>", now + 3600)));
        assert_eq!(v["allowed_mentions"], json!({ "parse": [] }), "mentions stay off");
        // Callbacks (command replies, button updates) go through the same path.
        assert_eq!(texts(&msg.to_callback(callback::MESSAGE)["data"]).len(), 1);
        // A message that already shows the line (maintenance refusal) does not get it twice.
        let line = notices::current_discord_line().unwrap();
        let shown = Message::new(vec![Component::text(line.clone())]);
        assert_eq!(texts(&shown.to_json()), vec![line]);

        // Not announced yet / ended: nothing.
        board.set_notices(vec![notice(2, Kind::Info, now + 7200, 10, now + 3600)]);
        assert!(texts(&msg.to_json()).is_empty());
        board.set_notices(vec![notice(3, Kind::Info, now - 7200, 10, now - 9000)]);
        assert!(texts(&msg.to_json()).is_empty());
        notices::uninstall();
    }

    #[test]
    fn notice_respects_discord_limits() {
        let full = Message::new(vec![Component::text("x".repeat(MAX_TEXT_TOTAL - 5))]);
        assert_eq!(full.with_notice("-# a long notice").components.len(), 1, "text total");
        let many = Message::new((0..MAX_COMPONENTS).map(|_| Component::separator()).collect());
        assert_eq!(many.with_notice("-# n").components.len(), MAX_COMPONENTS, "component count");
        let ok = Message::new(vec![Component::text("hi")]);
        assert_eq!(ok.with_notice("-# n").components.len(), 2);
    }
}
