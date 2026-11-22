//! Classify user input: YouTube / YouTube Music, Spotify, any other URL, or search text.

use reqwest::Url;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    YoutubeVideo { id: String, music: bool },
    YoutubePlaylist { id: String, music: bool },
    /// YouTube Music album page (`browse/MPREb_...`).
    YoutubeMusicAlbum(String),
    /// YouTube Music artist page (`channel/UC...`): its top songs.
    YoutubeMusicArtist(String),
    SpotifyTrack(String),
    SpotifyAlbum(String),
    SpotifyPlaylist(String),
    /// Spotify short link (spotify.link / spoti.fi) that must be followed first.
    SpotifyShortLink(String),
    /// Spotify URL of a kind we cannot play (artist, show, episode...).
    SpotifyUnsupported(String),
    /// Apple Music song (`/song/...` or `/album/...?i=<track>`), with its storefront.
    AppleSong { country: String, id: String },
    AppleAlbum { country: String, id: String },
    /// Public playlist (`pl.…`).
    ApplePlaylist { country: String, id: String },
    /// Apple Music URL of a kind we cannot play (artist, station...).
    AppleUnsupported(String),
    Url(String),
    Search(String),
}

pub fn detect(raw: &str) -> Input {
    let q = raw.trim();
    if let Some(rest) = q.strip_prefix("spotify:") {
        return spotify_uri(rest).unwrap_or_else(|| Input::SpotifyUnsupported(q.to_owned()));
    }
    let Some(url) = parse_url(q) else {
        return Input::Search(q.to_owned());
    };
    let host = url.host_str().unwrap_or("").trim_start_matches("www.").to_ascii_lowercase();
    match host.as_str() {
        "youtube.com" | "m.youtube.com" | "music.youtube.com" | "youtube-nocookie.com" | "youtu.be" => {
            youtube(&url, &host).unwrap_or_else(|| Input::Url(url.to_string()))
        }
        "open.spotify.com" | "play.spotify.com" => spotify(&url),
        "spotify.link" | "spoti.fi" => Input::SpotifyShortLink(url.to_string()),
        "music.apple.com" | "geo.music.apple.com" => apple(&url),
        _ => Input::Url(url.to_string()),
    }
}

/// Accept full URLs and bare `domain.tld/...` inputs; anything with spaces is a search.
fn parse_url(q: &str) -> Option<Url> {
    if q.is_empty() || q.contains(char::is_whitespace) {
        return None;
    }
    if q.starts_with("http://") || q.starts_with("https://") {
        return Url::parse(q).ok().filter(|u| u.host_str().is_some());
    }
    // Bare "youtube.com/watch?v=..." style input.
    let head = q.split(['/', '?']).next().unwrap_or("");
    let looks_like_host = head.contains('.')
        && head.rsplit('.').next().is_some_and(|tld| tld.len() >= 2 && tld.chars().all(|c| c.is_ascii_alphabetic()))
        && head.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
    if looks_like_host && q.contains('/') {
        return Url::parse(&format!("https://{q}")).ok();
    }
    None
}

fn query(url: &Url, key: &str) -> Option<String> {
    url.query_pairs().find(|(k, _)| k == key).map(|(_, v)| v.into_owned()).filter(|v| !v.is_empty())
}

pub fn is_video_id(s: &str) -> bool {
    s.len() == 11 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn is_channel_id(s: &str) -> bool {
    s.len() == 24 && s.starts_with("UC") && is_playlist_id(s)
}

fn is_playlist_id(s: &str) -> bool {
    s.len() >= 2 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn youtube(url: &Url, host: &str) -> Option<Input> {
    let music = host == "music.youtube.com";
    let segs: Vec<&str> = url.path_segments().map(|s| s.filter(|p| !p.is_empty()).collect()).unwrap_or_default();
    let list = query(url, "list").filter(|l| is_playlist_id(l));
    let video = if host == "youtu.be" {
        segs.first().map(|s| s.to_string())
    } else {
        match segs.as_slice() {
            ["watch", ..] => query(url, "v"),
            ["shorts" | "live" | "embed" | "v" | "e", id, ..] => Some(id.to_string()),
            _ => None,
        }
    }
    .filter(|v| is_video_id(v));

    match (video, list) {
        // Auto-generated mixes are endless; play the video the user clicked.
        (Some(id), Some(l)) if l.starts_with("RD") => Some(Input::YoutubeVideo { id, music }),
        (_, Some(id)) => Some(Input::YoutubePlaylist { id, music }),
        (Some(id), None) => Some(Input::YoutubeVideo { id, music }),
        (None, None) => match segs.as_slice() {
            // music.youtube.com/browse/VL<playlist>
            ["browse", b] if b.starts_with("VL") && b.len() > 2 => {
                Some(Input::YoutubePlaylist { id: b[2..].to_owned(), music })
            }
            ["browse", b] if music && b.starts_with("MPREb_") && is_playlist_id(b) => {
                Some(Input::YoutubeMusicAlbum(b.to_string()))
            }
            ["channel" | "browse", c] if music && is_channel_id(c) => Some(Input::YoutubeMusicArtist(c.to_string())),
            _ => None,
        },
    }
}

fn spotify(url: &Url) -> Input {
    let segs: Vec<&str> = url.path_segments().map(|s| s.filter(|p| !p.is_empty()).collect()).unwrap_or_default();
    // Drop locale prefixes ("intl-es") and the embed prefix.
    let segs: Vec<&str> =
        segs.into_iter().skip_while(|s| s.starts_with("intl-") || *s == "embed").collect();
    // Legacy user playlist URLs: /user/{u}/playlist/{id}
    let segs = match segs.as_slice() {
        ["user", _, rest @ ..] if !rest.is_empty() => rest.to_vec(),
        _ => segs,
    };
    match segs.as_slice() {
        [kind, id, ..] => spotify_kind(kind, id).unwrap_or_else(|| Input::SpotifyUnsupported(url.to_string())),
        _ => Input::SpotifyUnsupported(url.to_string()),
    }
}

fn apple(url: &Url) -> Input {
    let segs: Vec<&str> = url.path_segments().map(|s| s.filter(|p| !p.is_empty()).collect()).unwrap_or_default();
    // Optional storefront first: /us/album/..., /album/...
    let (country, rest) = match segs.split_first() {
        Some((cc, rest)) if cc.len() == 2 && cc.chars().all(|c| c.is_ascii_alphabetic()) => {
            (cc.to_ascii_lowercase(), rest.to_vec())
        }
        _ => ("us".to_owned(), segs.clone()),
    };
    let numeric = |s: &&str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
    let last = rest.last().copied().unwrap_or("");
    let found = match rest.first().copied() {
        Some("song") if rest.len() >= 2 && numeric(&last) => Some(Input::AppleSong { country, id: last.to_owned() }),
        Some("album") if rest.len() >= 2 && numeric(&last) => match query(url, "i").filter(|i| numeric(&i.as_str())) {
            Some(track) => Some(Input::AppleSong { country, id: track }),
            None => Some(Input::AppleAlbum { country, id: last.to_owned() }),
        },
        Some("playlist") if rest.len() >= 2 && last.starts_with("pl.") && is_playlist_id(&last.replace('.', "")) => {
            Some(Input::ApplePlaylist { country, id: last.to_owned() })
        }
        _ => None,
    };
    found.unwrap_or_else(|| Input::AppleUnsupported(url.to_string()))
}

fn spotify_uri(rest: &str) -> Option<Input> {
    let parts: Vec<&str> = rest.split(':').collect();
    let parts = match parts.as_slice() {
        ["user", _, tail @ ..] => tail.to_vec(),
        _ => parts,
    };
    match parts.as_slice() {
        [kind, id] => spotify_kind(kind, id),
        _ => None,
    }
}

fn spotify_kind(kind: &str, id: &str) -> Option<Input> {
    let valid = id.len() == 22 && id.chars().all(|c| c.is_ascii_alphanumeric());
    if !valid {
        return None;
    }
    let id = id.to_owned();
    match kind {
        "track" => Some(Input::SpotifyTrack(id)),
        "album" => Some(Input::SpotifyAlbum(id)),
        "playlist" => Some(Input::SpotifyPlaylist(id)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video(id: &str, music: bool) -> Input {
        Input::YoutubeVideo { id: id.into(), music }
    }
    fn playlist(id: &str, music: bool) -> Input {
        Input::YoutubePlaylist { id: id.into(), music }
    }

    #[test]
    fn youtube_videos() {
        for u in [
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://youtube.com/watch?v=dQw4w9WgXcQ&t=42s",
            "http://m.youtube.com/watch?feature=share&v=dQw4w9WgXcQ",
            "https://youtu.be/dQw4w9WgXcQ?si=abc",
            "https://www.youtube.com/shorts/dQw4w9WgXcQ",
            "https://www.youtube.com/live/dQw4w9WgXcQ?feature=share",
            "https://www.youtube-nocookie.com/embed/dQw4w9WgXcQ",
            "youtube.com/watch?v=dQw4w9WgXcQ",
            "  https://www.youtube.com/watch?v=dQw4w9WgXcQ  ",
        ] {
            assert_eq!(detect(u), video("dQw4w9WgXcQ", false), "{u}");
        }
        assert_eq!(detect("https://music.youtube.com/watch?v=dQw4w9WgXcQ&feature=share"), video("dQw4w9WgXcQ", true));
    }

    #[test]
    fn youtube_playlists() {
        let id = "PLFgquLnL59alCl_2TQvOiD5Vgm1hCaGSI";
        assert_eq!(detect(&format!("https://www.youtube.com/playlist?list={id}")), playlist(id, false));
        assert_eq!(detect(&format!("https://www.youtube.com/watch?v=dQw4w9WgXcQ&list={id}&index=3")), playlist(id, false));
        assert_eq!(detect(&format!("https://youtu.be/dQw4w9WgXcQ?list={id}")), playlist(id, false));
        assert_eq!(
            detect("https://music.youtube.com/playlist?list=OLAK5uy_kq0yXm5W2-Bf3Lr0b8YRZ9b2b3Q_Y8rXo"),
            playlist("OLAK5uy_kq0yXm5W2-Bf3Lr0b8YRZ9b2b3Q_Y8rXo", true)
        );
        assert_eq!(detect(&format!("https://music.youtube.com/browse/VL{id}")), playlist(id, true));
        // Mix with a video: play the video.
        assert_eq!(
            detect("https://www.youtube.com/watch?v=dQw4w9WgXcQ&list=RDdQw4w9WgXcQ&start_radio=1"),
            video("dQw4w9WgXcQ", false)
        );
        assert_eq!(detect("https://www.youtube.com/playlist?list=RDCLAK5uy_abc"), playlist("RDCLAK5uy_abc", false));
    }

    #[test]
    fn youtube_music_albums_and_artists() {
        assert_eq!(
            detect("https://music.youtube.com/browse/MPREb_NMmY1n1NJFS"),
            Input::YoutubeMusicAlbum("MPREb_NMmY1n1NJFS".into())
        );
        assert_eq!(
            detect("music.youtube.com/browse/MPREb_NMmY1n1NJFS?feature=share"),
            Input::YoutubeMusicAlbum("MPREb_NMmY1n1NJFS".into())
        );
        let artist = Input::YoutubeMusicArtist("UCk91oFc2hY2CdHirU93baLg".into());
        assert_eq!(detect("https://music.youtube.com/channel/UCk91oFc2hY2CdHirU93baLg"), artist);
        assert_eq!(detect("https://music.youtube.com/channel/UCk91oFc2hY2CdHirU93baLg?si=x"), artist);
        assert_eq!(detect("https://music.youtube.com/browse/UCk91oFc2hY2CdHirU93baLg"), artist);
        // Regular YouTube channels and malformed ids are not artist pages.
        assert!(matches!(detect("https://www.youtube.com/channel/UCk91oFc2hY2CdHirU93baLg"), Input::Url(_)));
        assert!(matches!(detect("https://music.youtube.com/channel/UCshort"), Input::Url(_)));
        assert!(matches!(detect("https://www.youtube.com/browse/MPREb_NMmY1n1NJFS"), Input::Url(_)));
    }

    #[test]
    fn youtube_other_pages_are_plain_urls() {
        assert!(matches!(detect("https://www.youtube.com/@SomeChannel"), Input::Url(_)));
        assert!(matches!(detect("https://www.youtube.com/watch?v=short"), Input::Url(_)));
    }

    #[test]
    fn spotify_urls_and_uris() {
        let id = "4uLU6hMCjMI75M1A2tKUQC";
        assert_eq!(detect(&format!("https://open.spotify.com/track/{id}?si=x")), Input::SpotifyTrack(id.into()));
        assert_eq!(detect(&format!("https://open.spotify.com/intl-es/track/{id}")), Input::SpotifyTrack(id.into()));
        assert_eq!(detect(&format!("https://open.spotify.com/intl-pt-BR/album/{id}")), Input::SpotifyAlbum(id.into()));
        assert_eq!(detect(&format!("https://open.spotify.com/playlist/{id}")), Input::SpotifyPlaylist(id.into()));
        assert_eq!(detect(&format!("https://open.spotify.com/embed/playlist/{id}")), Input::SpotifyPlaylist(id.into()));
        assert_eq!(
            detect(&format!("https://open.spotify.com/user/someone/playlist/{id}")),
            Input::SpotifyPlaylist(id.into())
        );
        assert_eq!(detect(&format!("open.spotify.com/album/{id}")), Input::SpotifyAlbum(id.into()));
        assert_eq!(detect(&format!("spotify:track:{id}")), Input::SpotifyTrack(id.into()));
        assert_eq!(detect(&format!("spotify:album:{id}")), Input::SpotifyAlbum(id.into()));
        assert_eq!(detect(&format!("spotify:user:someone:playlist:{id}")), Input::SpotifyPlaylist(id.into()));
        assert!(matches!(detect(&format!("https://open.spotify.com/artist/{id}")), Input::SpotifyUnsupported(_)));
        assert!(matches!(detect("spotify:track:bad"), Input::SpotifyUnsupported(_)));
        assert!(matches!(detect("https://spotify.link/AbCdEf"), Input::SpotifyShortLink(_)));
    }

    #[test]
    fn apple_music_urls() {
        let song = |cc: &str, id: &str| Input::AppleSong { country: cc.into(), id: id.into() };
        assert_eq!(detect("https://music.apple.com/us/song/moscow-mule/1622045625"), song("us", "1622045625"));
        assert_eq!(detect("https://music.apple.com/song/1622045625"), song("us", "1622045625"));
        assert_eq!(
            detect("https://music.apple.com/mx/album/moscow-mule/1622045624?i=1622045625&uo=4"),
            song("mx", "1622045625")
        );
        assert_eq!(
            detect("https://music.apple.com/us/album/un-verano-sin-ti/1622045624"),
            Input::AppleAlbum { country: "us".into(), id: "1622045624".into() }
        );
        assert_eq!(
            detect("music.apple.com/ES/album/un-verano-sin-ti/1622045624?l=en"),
            Input::AppleAlbum { country: "es".into(), id: "1622045624".into() }
        );
        assert_eq!(
            detect("https://music.apple.com/us/playlist/todays-hits/pl.f4d106fed2bd41149aaacabb233eb5eb"),
            Input::ApplePlaylist { country: "us".into(), id: "pl.f4d106fed2bd41149aaacabb233eb5eb".into() }
        );
        assert_eq!(
            detect("https://music.apple.com/gb/playlist/mine/pl.u-AkAmPlyUxKdxLv"),
            Input::ApplePlaylist { country: "gb".into(), id: "pl.u-AkAmPlyUxKdxLv".into() }
        );
        assert!(matches!(detect("https://music.apple.com/us/artist/bad-bunny/1126808565"), Input::AppleUnsupported(_)));
        assert!(matches!(detect("https://music.apple.com/us/album/x/notanumber"), Input::AppleUnsupported(_)));
    }

    #[test]
    fn other_urls_and_search() {
        assert_eq!(detect("https://soundcloud.com/a/b"), Input::Url("https://soundcloud.com/a/b".into()));
        assert_eq!(detect("bandcamp.com/track/x"), Input::Url("https://bandcamp.com/track/x".into()));
        assert_eq!(detect("bad bunny titi me pregunto"), Input::Search("bad bunny titi me pregunto".into()));
        assert_eq!(detect("despacito"), Input::Search("despacito".into()));
        assert_eq!(detect("AC/DC"), Input::Search("AC/DC".into()));
        assert_eq!(detect("Mr. Brightside"), Input::Search("Mr. Brightside".into()));
        assert_eq!(detect("USUM71703861"), Input::Search("USUM71703861".into()));
    }
}
