//! Small typed builder for Discord Components V2 messages (serenity 0.12 has no support for them).
//!
//! A message built here is sent as raw JSON through serenity's HTTP client. With the
//! `IS_COMPONENTS_V2` flag, `content` and `embeds` are not allowed and the flag cannot be removed
//! by later edits.

pub mod cards;

use serde_json::{json, Value};

pub const FLAG_EPHEMERAL: u64 = 1 << 6;
pub const FLAG_COMPONENTS_V2: u64 = 1 << 15;

/// Total text Discord accepts across all text displays of one message.
pub const MAX_TEXT_TOTAL: usize = 4000;

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
}

impl Button {
    pub fn new(style: ButtonStyle, label: impl Into<String>, custom_id: impl Into<String>) -> Self {
        Self {
            style,
            label: label.into(),
            target: custom_id.into(),
            disabled: false,
        }
    }

    pub fn link(label: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            style: ButtonStyle::Link,
            label: label.into(),
            target: url.into(),
            disabled: false,
        }
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    fn to_json(&self) -> Value {
        let mut v = json!({ "type": 2, "style": self.style as u8, "label": truncate(&self.label, 80) });
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

    fn to_json(&self) -> Value {
        match self {
            Self::Container { accent, children } => {
                let mut v = json!({ "type": 17, "components": children.iter().map(Self::to_json).collect::<Vec<_>>() });
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
                    Accessory::Button(b) => b.to_json(),
                },
            }),
            Self::Text(t) => json!({ "type": 10, "content": t }),
            Self::Separator { divider, large } => {
                json!({ "type": 14, "divider": divider, "spacing": if *large { 2 } else { 1 } })
            }
            Self::Row(buttons) => {
                json!({ "type": 1, "components": buttons.iter().take(5).map(Button::to_json).collect::<Vec<_>>() })
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
    pub fn to_json(&self) -> Value {
        debug_assert!(self.text_len() <= MAX_TEXT_TOTAL, "message text over the Discord limit");
        json!({
            "flags": self.flags(),
            "components": self.components.iter().map(Component::to_json).collect::<Vec<_>>(),
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

/// `[title](<url>)` with escaping; plain escaped title when the URL is not http(s).
pub fn link(title: &str, url: &str) -> String {
    let title = escape(&truncate(title, 200));
    if url.starts_with("https://") || url.starts_with("http://") {
        format!("[{title}](<{}>)", url.replace('>', "%3E"))
    } else {
        title
    }
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
        assert_eq!(truncate("abcdef", 5), "ab...");
        assert_eq!(truncate("abc", 5), "abc");
        assert_eq!(relative_time(5), "<t:5:R>");
        let ac = autocomplete_callback(&[("n".into(), "v".into()), ("n2".into(), "x".repeat(101))]);
        assert_eq!(ac["data"]["choices"].as_array().unwrap().len(), 1);
    }
}
