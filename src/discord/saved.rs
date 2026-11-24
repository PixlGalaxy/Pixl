//! `/playlist`: picking one of the caller's saved playlists (autocomplete choices, resolving what
//! was submitted, and the picker: every saved list, or the ones sharing the chosen name).
//!
//! Everything works on the caller's own lists as `Store::playlists(user)` returns them (pinned
//! first, then most recently used), so nobody else's lists can show up or be picked. Picker clicks
//! re-check the clicking user and re-read the candidates from that user's lists.

use std::collections::HashMap;

use super::{
    ids::{CustomId, PickAction, PickScope, PICKER_TTL_SECS},
    ui::{link, truncate, Button, ButtonStyle, Component, Icon, Message, Select, SelectOption, COLOR_INFO},
};
use crate::{i18n::I18n, store::SavedPlaylist};

/// Discord's limit for autocomplete choices and select menu options.
pub const MAX_CHOICES: usize = 25;
const MAX_LABEL: usize = 100;

/// Display name of a saved list's platform.
pub fn platform_label(platform: &str) -> Option<&'static str> {
    Some(match platform {
        "spotify" => "Spotify",
        "youtube" => "YouTube",
        "youtube_music" => "YouTube Music",
        "apple_music" => "Apple Music",
        "soundcloud" => "SoundCloud",
        _ => return None,
    })
}

/// The list's name, or its link when the platform gave none.
fn display_name(p: &SavedPlaylist) -> &str {
    let name = p.name.trim();
    if name.is_empty() {
        &p.url
    } else {
        name
    }
}

fn key(p: &SavedPlaylist) -> String {
    display_name(p).to_lowercase()
}

/// The lists with the same name as `p` (case-insensitive), `p` included, in the given order.
pub fn same_name<'a>(saved: &'a [SavedPlaylist], p: &SavedPlaylist) -> Vec<&'a SavedPlaylist> {
    let k = key(p);
    saved.iter().filter(|o| key(o) == k).collect()
}

/// The lists whose name another saved list shares (they need telling apart).
pub fn duplicated(saved: &[SavedPlaylist]) -> Vec<&SavedPlaylist> {
    let mut seen: HashMap<String, usize> = HashMap::new();
    for p in saved {
        *seen.entry(key(p)).or_default() += 1;
    }
    saved.iter().filter(|p| seen[&key(p)] > 1).collect()
}

/// `<name> · <Platform>` plus `extras`, in at most 100 characters (only the name is cut).
fn label(p: &SavedPlaylist, extras: &[String]) -> String {
    let mut suffix = String::new();
    for part in platform_label(&p.platform)
        .into_iter()
        .chain(extras.iter().map(String::as_str))
    {
        suffix.push_str(" · ");
        suffix.push_str(part);
    }
    let room = MAX_LABEL.saturating_sub(suffix.chars().count()).max(10);
    truncate(&format!("{}{suffix}", truncate(display_name(p), room)), MAX_LABEL)
}

fn songs(count: usize, i18n: &I18n) -> String {
    i18n.t("bot.playlist.songs", &[("count", &count.to_string())])
}

/// Autocomplete choices (label, id) for what the user typed: names starting with it first, then
/// names containing it (case-insensitive), each group in the given order; at most 25. Lists that
/// share a name also show their song count (`songs`: known counts by id, from the list cache), or
/// their place among the namesakes (`#2`) when it is not known.
pub fn choices(
    saved: &[SavedPlaylist],
    typed: &str,
    songs_by_id: &HashMap<i64, usize>,
    i18n: &I18n,
) -> Vec<(String, String)> {
    let q = typed.trim().to_lowercase();
    let names: Vec<String> = saved.iter().map(key).collect();
    let dupes: Vec<i64> = duplicated(saved).iter().map(|p| p.id).collect();
    let extras = |p: &SavedPlaylist| -> Vec<String> {
        if !dupes.contains(&p.id) {
            return Vec::new();
        }
        match songs_by_id.get(&p.id) {
            Some(n) => vec![songs(*n, i18n)],
            None => {
                let nth = same_name(saved, p).iter().position(|o| o.id == p.id).unwrap_or(0) + 1;
                vec![format!("#{nth}")]
            }
        }
    };
    let prefix = saved.iter().zip(&names).filter(|(_, n)| n.starts_with(&q));
    let inside = saved
        .iter()
        .zip(&names)
        .filter(|(_, n)| !n.starts_with(&q) && n.contains(&q));
    prefix
        .chain(inside)
        .take(MAX_CHOICES)
        .map(|(p, _)| (label(p, &extras(p)), p.id.to_string()))
        .collect()
}

/// The saved list a submitted value means: a picked choice (its id), else typed text matched
/// against the names: an exact (case-insensitive) name, else a prefix of a single name (lists
/// sharing that name count as one: the picker tells them apart). `None` when nothing (or more than
/// one name) matches.
pub fn resolve<'a>(saved: &'a [SavedPlaylist], value: &str) -> Option<&'a SavedPlaylist> {
    let v = value.trim();
    if v.is_empty() {
        return None;
    }
    if let Ok(id) = v.parse::<i64>() {
        if let Some(p) = saved.iter().find(|p| p.id == id) {
            return Some(p);
        }
    }
    let q = v.to_lowercase();
    if let Some(p) = saved.iter().find(|p| key(p) == q) {
        return Some(p);
    }
    let mut prefix = saved.iter().filter(|p| key(p).starts_with(&q));
    let first = prefix.next()?;
    let k = key(first);
    prefix.all(|p| key(p) == k).then_some(first)
}

// ---------------------------------------------------------------------------
// Picker
// ---------------------------------------------------------------------------

/// What `/playlist` does with the caller's saved lists and the `name` it got.
#[derive(Debug, PartialEq)]
pub enum Request<'a> {
    /// No saved playlists at all.
    Empty,
    /// One list: queue it.
    Play(&'a SavedPlaylist),
    /// Open the picker on `start`. `unmatched`: a name was given but matched nothing.
    Pick {
        scope: PickScope,
        start: &'a SavedPlaylist,
        unmatched: bool,
    },
}

/// No name: browse every list. A name that is one list: play it. A name shared by several lists:
/// pick among them. A name that matches nothing: browse every list.
pub fn decide<'a>(saved: &'a [SavedPlaylist], name: Option<&str>) -> Request<'a> {
    let Some(first) = saved.first() else {
        return Request::Empty;
    };
    let browse = |unmatched| Request::Pick {
        scope: PickScope::All,
        start: first,
        unmatched,
    };
    let Some(name) = name.map(str::trim).filter(|n| !n.is_empty()) else {
        return browse(false);
    };
    match resolve(saved, name) {
        Some(p) if same_name(saved, p).len() == 1 => Request::Play(p),
        Some(p) => Request::Pick {
            scope: PickScope::SameName,
            start: p,
            unmatched: false,
        },
        None => browse(true),
    }
}

/// The lists a picker pages through, from the user's current saved lists (`shown`: the id on
/// screen, which names the group for [`PickScope::SameName`]; empty when it is gone).
pub fn candidates(saved: &[SavedPlaylist], scope: PickScope, shown: i64) -> Vec<&SavedPlaylist> {
    match scope {
        PickScope::All => saved.iter().collect(),
        PickScope::SameName => match saved.iter().find(|p| p.id == shown) {
            Some(p) => same_name(saved, p),
            None => Vec::new(),
        },
    }
}

/// The index after a Previous / Next click, wrapping around (Select and Jump stay).
pub fn step(index: usize, len: usize, action: PickAction) -> usize {
    if len == 0 {
        return 0;
    }
    let index = index.min(len - 1);
    match action {
        PickAction::Previous => (index + len - 1) % len,
        PickAction::Next => (index + 1) % len,
        PickAction::Select | PickAction::Jump => index,
    }
}

/// Where a picker click leads among `cands`: Previous / Next from the shown list (or the first
/// when it is gone), Jump to the chosen id (`jump`, one of `cands`; else stay), Select the shown
/// list. `None` when there is nothing to show or select.
pub fn navigate(cands: &[&SavedPlaylist], shown: i64, action: PickAction, jump: Option<&str>) -> Option<usize> {
    if cands.is_empty() {
        return None;
    }
    let current = cands.iter().position(|p| p.id == shown);
    match action {
        PickAction::Previous | PickAction::Next => Some(step(current.unwrap_or(0), cands.len(), action)),
        PickAction::Jump => {
            let target = jump
                .and_then(|v| v.trim().parse::<i64>().ok())
                .and_then(|id| cands.iter().position(|p| p.id == id));
            Some(target.or(current).unwrap_or(0))
        }
        PickAction::Select => current,
    }
}

/// Whether a click on a picker may go on: `Err(error key)` when someone else clicked or it expired.
pub fn check_click(owner: u64, clicker: u64, issued: i64, now: i64) -> Result<(), &'static str> {
    if owner != clicker {
        Err("playlist_not_yours")
    } else if now - issued > PICKER_TTL_SECS {
        Err("playlist_expired")
    } else {
        Ok(())
    }
}

/// The picker: one list at a time among `candidates`.
#[derive(Debug, Clone)]
pub struct Picker<'a> {
    pub candidates: &'a [&'a SavedPlaylist],
    /// The one shown.
    pub index: usize,
    pub scope: PickScope,
    /// Who ran `/playlist`, and when (unix seconds).
    pub user_id: u64,
    pub issued: i64,
    /// Known song counts by id (the list cache; nothing is fetched for the picker).
    pub songs: &'a HashMap<i64, usize>,
    /// A line above the list ("No saved playlist matches ...").
    pub note: Option<String>,
}

impl Picker<'_> {
    fn custom_id(&self, action: PickAction, shown: i64) -> String {
        CustomId::PlaylistPick {
            action,
            shown,
            scope: self.scope,
            user_id: self.user_id,
            issued: self.issued,
        }
        .encode()
    }

    /// `#k` among the candidates with the same name, when there are several.
    fn nth(&self, p: &SavedPlaylist) -> Option<String> {
        let k = key(p);
        let same: Vec<i64> = self.candidates.iter().filter(|o| key(o) == k).map(|o| o.id).collect();
        (same.len() > 1).then(|| format!("#{}", same.iter().position(|id| *id == p.id).unwrap_or(0) + 1))
    }
}

/// "Select the playlist": the shown list (cover, name, platform, songs, `k / N`), a "Jump to a
/// playlist" menu and Previous / Select / Next. Ephemeral.
pub fn picker_card(v: &Picker, i18n: &I18n) -> Message {
    let n = v.candidates.len().max(1);
    let index = v.index.min(n - 1);
    let shown_id = v.candidates.get(index).map_or(0, |p| p.id);
    let mut children = Vec::new();
    if let Some(note) = &v.note {
        children.push(Component::text(format!("-# {note}")));
    }
    if let Some(p) = v.candidates.get(index) {
        let mut meta: Vec<String> = platform_label(&p.platform).map(str::to_owned).into_iter().collect();
        if let Some(c) = v.songs.get(&p.id) {
            meta.push(songs(*c, i18n));
        }
        meta.push(format!("{} / {n}", index + 1));
        let texts = vec![
            format!("-# {}", i18n.t("bot.playlist.pick_title", &[])),
            format!("### {}", link(display_name(p), &p.url)),
            format!("-# {}", meta.join(" · ")),
        ];
        children.extend(Component::section_or_text(texts, p.image_url.as_deref()));
    }
    if v.candidates.len() > 1 {
        let options = v
            .candidates
            .iter()
            .take(MAX_CHOICES)
            .map(|p| {
                let extra = match v.songs.get(&p.id) {
                    Some(c) => Some(songs(*c, i18n)),
                    None => v.nth(p),
                };
                SelectOption {
                    label: label(p, &extra.into_iter().collect::<Vec<_>>()),
                    value: p.id.to_string(),
                    default: p.id == shown_id,
                }
            })
            .collect();
        children.push(Component::Select(Select {
            custom_id: v.custom_id(PickAction::Jump, shown_id),
            placeholder: i18n.t("bot.playlist.jump", &[]),
            options,
        }));
    }
    children.push(Component::Row(vec![
        Button::new(
            ButtonStyle::Secondary,
            i18n.t("bot.button.previous", &[]),
            v.custom_id(PickAction::Previous, shown_id),
        )
        .icon(Icon::Previous),
        Button::new(
            ButtonStyle::Primary,
            i18n.t("bot.button.select", &[]),
            v.custom_id(PickAction::Select, shown_id),
        )
        .icon(Icon::Select),
        Button::new(
            ButtonStyle::Secondary,
            i18n.t("bot.button.next", &[]),
            v.custom_id(PickAction::Next, shown_id),
        )
        .icon(Icon::Next),
    ]));
    Message::new(vec![Component::Container {
        accent: Some(COLOR_INFO),
        children,
    }])
    .ephemeral(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pl(id: i64, name: &str, platform: &str) -> SavedPlaylist {
        SavedPlaylist {
            id,
            url: format!("https://open.spotify.com/playlist/{id}"),
            platform: platform.into(),
            name: name.into(),
            image_url: Some(format!("https://i.scdn.co/image/{id}")),
            pinned: false,
            added_at: 0,
            used_at: 0,
        }
    }

    fn en() -> I18n {
        I18n::load("en").unwrap()
    }

    fn names(c: &[(String, String)]) -> Vec<&str> {
        c.iter().map(|(n, _)| n.as_str()).collect()
    }

    fn all(saved: &[SavedPlaylist], typed: &str) -> Vec<(String, String)> {
        choices(saved, typed, &HashMap::new(), &en())
    }

    #[test]
    fn labels_name_the_platform_and_fit_discord() {
        let one = |p: SavedPlaylist| names(&all(&[p], ""))[0].to_owned();
        assert_eq!(one(pl(1, "Road trip", "spotify")), "Road trip · Spotify");
        assert_eq!(one(pl(1, "Mix", "youtube_music")), "Mix · YouTube Music");
        assert_eq!(one(pl(1, "Mix", "apple_music")), "Mix · Apple Music");
        assert_eq!(one(pl(1, "Mix", "youtube")), "Mix · YouTube");
        assert_eq!(one(pl(1, "Mix", "soundcloud")), "Mix · SoundCloud");
        assert_eq!(one(pl(1, "Mix", "other")), "Mix");
        let long = one(pl(1, &"a".repeat(300), "youtube_music"));
        assert_eq!(long.chars().count(), 100);
        assert!(long.ends_with("... · YouTube Music"), "{long}");
        // No name: the link stands in.
        assert!(one(pl(7, " ", "spotify")).starts_with("https://open.spotify.com/playlist/7"));
    }

    #[test]
    fn namesakes_are_told_apart_in_the_labels() {
        let saved = vec![
            pl(1, "Mix", "spotify"),
            pl(2, "mix", "spotify"),
            pl(3, "Mix", "youtube"),
            pl(4, "Gym", "spotify"),
        ];
        let counts = HashMap::from([(1, 42), (3, 7)]);
        let c = choices(&saved, "", &counts, &en());
        assert_eq!(
            names(&c),
            [
                "Mix · Spotify · 42 songs",
                "mix · Spotify · #2",
                "Mix · YouTube · 7 songs",
                "Gym · Spotify"
            ]
        );
        let long = choices(
            &[pl(1, &"a".repeat(300), "spotify"), pl(2, &"A".repeat(300), "spotify")],
            "",
            &counts,
            &en(),
        );
        assert_eq!(long[0].0.chars().count(), 100);
        assert!(long[0].0.ends_with(" · Spotify · 42 songs"), "{}", long[0].0);
        assert!(long[1].0.ends_with(" · Spotify · #2"), "{}", long[1].0);
        let ids: Vec<i64> = duplicated(&saved).iter().map(|p| p.id).collect();
        assert_eq!(ids, [1, 2, 3]);
    }

    #[test]
    fn choices_prefix_first_then_substring_in_saved_order() {
        // Already pinned first, then most recently used (the store's order).
        let saved = vec![
            pl(1, "Chill evening", "spotify"),
            pl(2, "Gym", "youtube"),
            pl(3, "Late night chill", "youtube_music"),
            pl(4, "chillhop", "apple_music"),
        ];
        let c = all(&saved, "CHILL");
        assert_eq!(
            names(&c),
            [
                "Chill evening · Spotify",
                "chillhop · Apple Music",
                "Late night chill · YouTube Music"
            ]
        );
        assert_eq!(c[0].1, "1", "the value is the id");
        // Nothing typed: every list, in order.
        let ids: Vec<String> = all(&saved, "").into_iter().map(|(_, v)| v).collect();
        assert_eq!(ids, ["1", "2", "3", "4"]);
        assert!(all(&saved, "jazz").is_empty());
        assert!(all(&[], "").is_empty(), "no saved lists: no choices");
        // At most 25.
        let many: Vec<_> = (0..40).map(|i| pl(i, &format!("List {i}"), "spotify")).collect();
        assert_eq!(all(&many, "list").len(), MAX_CHOICES);
    }

    #[test]
    fn resolve_picked_ids_and_typed_names() {
        let saved = vec![
            pl(10, "Road trip", "spotify"),
            pl(11, "Rock classics", "youtube"),
            pl(12, "Rock", "youtube_music"),
            pl(13, "Jazz", "spotify"),
            pl(14, "Jazz", "youtube"),
        ];
        assert_eq!(resolve(&saved, "13").map(|p| p.id), Some(13), "a picked choice");
        assert_eq!(resolve(&saved, "rock").map(|p| p.id), Some(12), "exact name first");
        assert_eq!(resolve(&saved, " ROAD ").map(|p| p.id), Some(10), "the only prefix");
        assert_eq!(resolve(&saved, "Rock c").map(|p| p.id), Some(11));
        assert_eq!(
            resolve(&saved, "ja").map(|p| p.id),
            Some(13),
            "namesakes count as one name"
        );
        assert_eq!(resolve(&saved, "r"), None, "ambiguous prefix");
        assert_eq!(resolve(&saved, "blues"), None);
        assert_eq!(resolve(&saved, ""), None);
        // An id that is not one of the caller's lists (someone else's) is refused.
        assert_eq!(resolve(&saved, "99"), None);
        assert_eq!(resolve(&[], "10"), None);
        // Namesakes of a resolved list (the picker's candidates).
        let jazz = resolve(&saved, "jazz").unwrap();
        let ids: Vec<i64> = same_name(&saved, jazz).iter().map(|p| p.id).collect();
        assert_eq!(ids, [13, 14]);
        assert_eq!(same_name(&saved, &saved[0]).len(), 1);
    }

    #[tokio::test]
    async fn only_the_callers_lists_are_offered_or_resolved() {
        use crate::store::{NewPlaylist, SaveOutcome, Store};
        let store = Store::open_in_memory().await.unwrap();
        let save = |url: &'static str, name: &'static str| NewPlaylist {
            url,
            platform: "spotify",
            name,
            image_url: None,
        };
        store.save_playlist(1, &save("https://a", "Mine"), 20).await.unwrap();
        let SaveOutcome::Saved { id: theirs, .. } =
            store.save_playlist(2, &save("https://b", "Theirs"), 20).await.unwrap()
        else {
            panic!("saved");
        };
        let mine = store.playlists(1).await.unwrap();
        assert_eq!(names(&all(&mine, "")), ["Mine · Spotify"]);
        assert!(all(&mine, "theirs").is_empty());
        assert!(resolve(&mine, &theirs.to_string()).is_none(), "another user's id");
        assert!(resolve(&mine, "Theirs").is_none());
        // A picker click re-reads each candidate as the clicking user: someone else's id is gone.
        assert!(store.playlist(1, theirs).await.unwrap().is_none());
    }

    #[test]
    fn picker_pages_wrap_around() {
        use PickAction::*;
        assert_eq!(step(0, 3, Next), 1);
        assert_eq!(step(2, 3, Next), 0, "wraps forward");
        assert_eq!(step(0, 3, Previous), 2, "wraps back");
        assert_eq!(step(1, 3, Previous), 0);
        assert_eq!(step(1, 3, Select), 1);
        assert_eq!(step(7, 3, Next), 0, "an index past the end is clamped first");
        assert_eq!(step(0, 1, Next), 0);
        assert_eq!(step(0, 0, Next), 0);

        let saved = [pl(5, "A", "spotify"), pl(6, "B", "spotify"), pl(7, "C", "youtube")];
        let c: Vec<&SavedPlaylist> = saved.iter().collect();
        assert_eq!(navigate(&c, 7, Next, None), Some(0));
        assert_eq!(navigate(&c, 5, Previous, None), Some(2));
        assert_eq!(navigate(&c, 99, Next, None), Some(1), "shown list gone: from the first");
        assert_eq!(navigate(&c, 6, Select, None), Some(1));
        assert_eq!(
            navigate(&c, 99, Select, None),
            None,
            "a list that is gone is never selected"
        );
        assert_eq!(navigate(&[], 5, Next, None), None);
    }

    #[test]
    fn picker_dropdown_jumps_only_to_candidates() {
        let saved = [pl(5, "A", "spotify"), pl(6, "B", "spotify"), pl(7, "C", "youtube")];
        let c: Vec<&SavedPlaylist> = saved.iter().collect();
        assert_eq!(navigate(&c, 5, PickAction::Jump, Some("7")), Some(2));
        assert_eq!(
            navigate(&c, 6, PickAction::Jump, Some("42")),
            Some(1),
            "not a candidate: stay"
        );
        assert_eq!(navigate(&c, 6, PickAction::Jump, Some("x")), Some(1));
        assert_eq!(navigate(&c, 6, PickAction::Jump, None), Some(1));
    }

    #[test]
    fn decide_browse_pick_or_play() {
        let saved = vec![
            pl(10, "Road trip", "spotify"),
            pl(11, "Jazz", "spotify"),
            pl(12, "jazz", "youtube"),
            pl(13, "Gym", "spotify"),
        ];
        assert_eq!(decide(&[], None), Request::Empty);
        assert_eq!(decide(&[], Some("jazz")), Request::Empty);
        // No name: browse everything from the first.
        assert_eq!(
            decide(&saved, None),
            Request::Pick {
                scope: PickScope::All,
                start: &saved[0],
                unmatched: false
            }
        );
        assert_eq!(decide(&saved, Some("  ")), decide(&saved, None));
        // A name that matches nothing: browse everything, saying so.
        assert_eq!(
            decide(&saved, Some("blues")),
            Request::Pick {
                scope: PickScope::All,
                start: &saved[0],
                unmatched: true
            }
        );
        // Exactly one list: play it directly.
        assert_eq!(decide(&saved, Some("gym")), Request::Play(&saved[3]));
        assert_eq!(decide(&saved, Some("10")), Request::Play(&saved[0]), "a picked choice");
        // Several lists with that name (typed or picked): pick among them, from the one chosen.
        assert_eq!(
            decide(&saved, Some("JAZZ")),
            Request::Pick {
                scope: PickScope::SameName,
                start: &saved[1],
                unmatched: false
            }
        );
        assert_eq!(
            decide(&saved, Some("12")),
            Request::Pick {
                scope: PickScope::SameName,
                start: &saved[2],
                unmatched: false
            }
        );
        let ids = |v: Vec<&SavedPlaylist>| v.iter().map(|p| p.id).collect::<Vec<_>>();
        assert_eq!(ids(candidates(&saved, PickScope::SameName, 12)), [11, 12]);
        assert_eq!(ids(candidates(&saved, PickScope::All, 12)), [10, 11, 12, 13]);
        assert!(
            candidates(&saved, PickScope::SameName, 99).is_empty(),
            "gone (or not theirs)"
        );
    }

    #[test]
    fn picker_clicks_are_for_the_owner_and_expire() {
        assert_eq!(check_click(1, 1, 1_000, 1_000 + PICKER_TTL_SECS), Ok(()));
        assert_eq!(check_click(1, 2, 1_000, 1_000), Err("playlist_not_yours"));
        assert_eq!(
            check_click(1, 1, 1_000, 1_001 + PICKER_TTL_SECS),
            Err("playlist_expired")
        );
        // Someone else gets "not yours" even after expiry.
        assert_eq!(check_click(1, 2, 0, 1_000_000), Err("playlist_not_yours"));
    }

    #[test]
    fn picker_card_shows_one_candidate_a_menu_and_buttons() {
        let saved = [
            pl(13, "Jazz", "spotify"),
            pl(14, "Jazz", "youtube"),
            pl(15, "jazz", "apple_music"),
        ];
        let refs: Vec<&SavedPlaylist> = saved.iter().collect();
        let counts = HashMap::from([(14, 12)]);
        let v = Picker {
            candidates: &refs,
            index: 1,
            scope: PickScope::SameName,
            user_id: 42,
            issued: 1_700_000_000,
            songs: &counts,
            note: None,
        };
        let json = picker_card(&v, &en()).to_json_with(&|_| None);
        let eph = crate::discord::ui::FLAG_EPHEMERAL;
        assert_eq!(json["flags"].as_u64().unwrap() & eph, eph);
        let container = &json["components"][0];
        let section = &container["components"][0];
        assert_eq!(section["type"], 9);
        assert_eq!(section["accessory"]["media"]["url"], "https://i.scdn.co/image/14");
        let text: Vec<&str> = section["components"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["content"].as_str().unwrap())
            .collect();
        assert_eq!(text[0], "-# Select the playlist");
        assert!(text[1].contains("Jazz"), "{}", text[1]);
        assert_eq!(text[2], "-# YouTube · 12 songs · 2 / 3");

        // The menu: every candidate, told apart, the shown one selected.
        let menu = &container["components"][1]["components"][0];
        assert_eq!(menu["type"], 3);
        assert_eq!(menu["placeholder"], "Jump to a playlist");
        let labels: Vec<&str> = menu["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o["label"].as_str().unwrap())
            .collect();
        assert_eq!(
            labels,
            [
                "Jazz · Spotify · #1",
                "Jazz · YouTube · 12 songs",
                "jazz · Apple Music · #3"
            ]
        );
        assert_eq!(menu["options"][1]["default"], true);
        assert_eq!(menu["options"][1]["value"], "14");

        let row = &container["components"][2]["components"];
        let mut seen = vec![CustomId::parse(menu["custom_id"].as_str().unwrap()).unwrap()];
        seen.extend((0..3).map(|i| CustomId::parse(row[i]["custom_id"].as_str().unwrap()).unwrap()));
        let actions: Vec<PickAction> = seen
            .iter()
            .map(|c| match c {
                CustomId::PlaylistPick {
                    action,
                    shown,
                    scope,
                    user_id,
                    issued,
                } => {
                    assert_eq!(
                        (*shown, *scope, *user_id, *issued),
                        (14, PickScope::SameName, 42, 1_700_000_000)
                    );
                    *action
                }
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            actions,
            [
                PickAction::Jump,
                PickAction::Previous,
                PickAction::Select,
                PickAction::Next
            ]
        );
        assert_eq!(row[1]["style"], 1, "Select is the primary button");
        assert_eq!((row[0]["style"].as_u64(), row[2]["style"].as_u64()), (Some(2), Some(2)));
    }

    #[test]
    fn picker_menu_is_capped_and_ids_stay_short() {
        let many: Vec<SavedPlaylist> = (0..40)
            .map(|i| pl(1_000_000 + i, &format!("List {i}"), "spotify"))
            .collect();
        let refs: Vec<&SavedPlaylist> = many.iter().collect();
        let none = HashMap::new();
        let v = Picker {
            candidates: &refs,
            index: 39,
            scope: PickScope::All,
            user_id: u64::MAX,
            issued: 1_700_000_000,
            songs: &none,
            note: Some("No match".into()),
        };
        let json = picker_card(&v, &en()).to_json_with(&|_| None);
        let c = &json["components"][0]["components"];
        assert_eq!(c[0]["content"], "-# No match");
        assert!(c[1].to_string().contains("40 / 40"));
        assert_eq!(c[2]["components"][0]["options"].as_array().unwrap().len(), MAX_CHOICES);
        for b in c[3]["components"].as_array().unwrap() {
            assert!(b["custom_id"].as_str().unwrap().len() <= 100);
        }
        // One list: no menu.
        let one = [&many[0]];
        let v = Picker {
            candidates: &one,
            index: 0,
            note: None,
            ..v
        };
        let json = picker_card(&v, &en()).to_json_with(&|_| None);
        assert_eq!(json["components"][0]["components"].as_array().unwrap().len(), 2);
    }
}
