//! Fixed genre taxonomy and normalization of raw tags (Last.fm, MusicBrainz, Ollama) into it.
//!
//! Every tag stored in `track_tags` / `artist_tags` is one of [`GENRES`]. Raw tags are normalized
//! with [`crate::sources::text::normalize`] (lowercase, no accents, punctuation as spaces), then:
//! exact synonym -> junk -> word/phrase rules -> unknown (optionally mapped once by Ollama).

use std::collections::HashMap;
use std::sync::OnceLock;

use crate::sources::text::{contains_phrase, normalize};

/// Canonical genres. Display names, lowercase.
pub const GENRES: &[&str] = &[
    // Pop / rock
    "pop",
    "synthpop",
    "k-pop",
    "j-pop",
    "rock",
    "classic rock",
    "hard rock",
    "alternative",
    "indie",
    "punk",
    "emo",
    "grunge",
    "metal",
    // Urban
    "hip hop",
    "trap",
    "drill",
    "r&b",
    "soul",
    "funk",
    "disco",
    // Electronic
    "electronic",
    "edm",
    "house",
    "techno",
    "trance",
    "drum and bass",
    "dubstep",
    "ambient",
    "chillout",
    "lo-fi",
    // Traditional / others
    "jazz",
    "blues",
    "classical",
    "soundtrack",
    "anime",
    "folk",
    "country",
    "singer-songwriter",
    "acoustic",
    "instrumental",
    "gospel",
    "christian",
    "children",
    "world",
    "experimental",
    // Caribbean / African
    "reggae",
    "dancehall",
    "afrobeats",
    // Latin
    "latin",
    "latin pop",
    "reggaeton",
    "latin trap",
    "dembow",
    "bachata",
    "salsa",
    "merengue",
    "cumbia",
    "vallenato",
    "regional mexican",
    "bolero",
    "tango",
    "flamenco",
    "rock en espanol",
    // Brazil
    "bossa nova",
    "samba",
    "sertanejo",
    "brazilian funk",
];

/// Result of normalizing one raw tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TagMatch {
    /// One or more canonical genres (e.g. "indie rock" -> indie, rock).
    Genres(Vec<&'static str>),
    /// Not a genre ("seen live", "female vocalists", "2010s", nationalities, moods).
    Junk,
    /// Looks like a genre we do not know; candidates for the Ollama mapping.
    Unknown,
}

/// Whole normalized tag -> canonical genres. Checked first, short-circuits.
const SYNONYMS: &[(&str, &[&str])] = &[
    ("hip hop", &["hip hop"]),
    ("hiphop", &["hip hop"]),
    ("rap", &["hip hop"]),
    ("hip hop rap", &["hip hop"]),
    ("gangsta rap", &["hip hop"]),
    ("boom bap", &["hip hop"]),
    ("grime", &["hip hop"]),
    ("phonk", &["hip hop"]),
    ("r b", &["r&b"]),
    ("rnb", &["r&b"]),
    ("rhythm and blues", &["r&b"]),
    ("contemporary r b", &["r&b"]),
    ("neo soul", &["soul", "r&b"]),
    ("drum and bass", &["drum and bass"]),
    ("drum n bass", &["drum and bass"]),
    ("drum bass", &["drum and bass"]),
    ("dnb", &["drum and bass"]),
    ("jungle", &["drum and bass"]),
    ("lo fi", &["lo-fi"]),
    ("lofi", &["lo-fi"]),
    ("lo fi hip hop", &["lo-fi"]),
    ("chillhop", &["lo-fi"]),
    ("chill", &["chillout"]),
    ("chill out", &["chillout"]),
    ("chillout", &["chillout"]),
    ("downtempo", &["chillout"]),
    ("trip hop", &["chillout"]),
    ("lounge", &["chillout"]),
    ("synth pop", &["synthpop"]),
    ("synthpop", &["synthpop"]),
    ("electropop", &["synthpop"]),
    ("new wave", &["synthpop"]),
    ("synthwave", &["synthpop", "electronic"]),
    ("k pop", &["k-pop"]),
    ("kpop", &["k-pop"]),
    ("j pop", &["j-pop"]),
    ("jpop", &["j-pop"]),
    ("j rock", &["j-pop", "rock"]),
    ("hard rock", &["hard rock"]),
    ("classic rock", &["classic rock"]),
    ("rock and roll", &["classic rock"]),
    ("rock n roll", &["classic rock"]),
    ("oldies", &["classic rock"]),
    ("alternative rock", &["alternative", "rock"]),
    ("alt rock", &["alternative", "rock"]),
    ("britpop", &["indie", "rock"]),
    ("shoegaze", &["alternative"]),
    ("hardcore", &["punk"]),
    ("post hardcore", &["punk"]),
    ("pop punk", &["punk", "pop"]),
    ("metalcore", &["metal"]),
    ("deathcore", &["metal"]),
    ("nu metal", &["metal"]),
    ("electronic dance music", &["edm"]),
    ("dance", &["edm"]),
    ("big room", &["edm"]),
    ("hardstyle", &["edm"]),
    ("future bass", &["edm"]),
    ("electronica", &["electronic"]),
    ("electro", &["electronic"]),
    ("idm", &["electronic", "experimental"]),
    ("singer songwriter", &["singer-songwriter"]),
    ("cantautor", &["singer-songwriter"]),
    ("cantautora", &["singer-songwriter"]),
    ("cantautores", &["singer-songwriter"]),
    ("acustico", &["acoustic"]),
    ("piano", &["instrumental"]),
    ("orchestral", &["classical"]),
    ("opera", &["classical"]),
    ("clasica", &["classical"]),
    ("musica clasica", &["classical"]),
    ("soundtrack", &["soundtrack"]),
    ("ost", &["soundtrack"]),
    ("score", &["soundtrack"]),
    ("film score", &["soundtrack"]),
    ("video game music", &["soundtrack"]),
    ("game music", &["soundtrack"]),
    ("anime", &["anime"]),
    ("worship", &["christian"]),
    ("ccm", &["christian"]),
    ("musica cristiana", &["christian"]),
    ("cristiana", &["christian"]),
    ("children s music", &["children"]),
    ("kids", &["children"]),
    ("infantil", &["children"]),
    ("musica infantil", &["children"]),
    ("world music", &["world"]),
    ("kizomba", &["world"]),
    ("zouk", &["world"]),
    ("ska", &["reggae"]),
    ("dub", &["reggae"]),
    ("roots reggae", &["reggae"]),
    ("afrobeat", &["afrobeats"]),
    ("afropop", &["afrobeats"]),
    ("amapiano", &["afrobeats"]),
    // Latin
    ("latin", &["latin"]),
    ("latino", &["latin"]),
    ("latin music", &["latin"]),
    ("musica latina", &["latin"]),
    ("tropical", &["latin"]),
    ("champeta", &["latin"]),
    ("guaracha", &["latin"]),
    ("latin pop", &["latin pop"]),
    ("pop latino", &["latin pop"]),
    ("pop en espanol", &["latin pop"]),
    ("balada", &["latin pop"]),
    ("baladas", &["latin pop"]),
    ("balada romantica", &["latin pop"]),
    ("latin ballad", &["latin pop"]),
    ("reggaeton", &["reggaeton"]),
    ("regueton", &["reggaeton"]),
    ("reggaeton latino", &["reggaeton"]),
    ("perreo", &["reggaeton"]),
    ("neoperreo", &["reggaeton"]),
    ("urbano", &["reggaeton"]),
    ("urbano latino", &["reggaeton"]),
    ("latin urban", &["reggaeton"]),
    ("musica urbana", &["reggaeton"]),
    ("latin trap", &["latin trap"]),
    ("trap latino", &["latin trap"]),
    ("trap en espanol", &["latin trap"]),
    ("spanish trap", &["latin trap"]),
    ("trap argentino", &["latin trap"]),
    ("dembow", &["dembow"]),
    ("dominican dembow", &["dembow"]),
    ("bachata", &["bachata"]),
    ("salsa", &["salsa"]),
    ("son cubano", &["salsa"]),
    ("timba", &["salsa"]),
    ("salsa romantica", &["salsa"]),
    ("merengue", &["merengue"]),
    ("cumbia", &["cumbia"]),
    ("cuarteto", &["cumbia"]),
    ("chicha", &["cumbia"]),
    ("vallenato", &["vallenato"]),
    ("regional mexican", &["regional mexican"]),
    ("regional mexicano", &["regional mexican"]),
    ("musica mexicana", &["regional mexican"]),
    ("musica regional mexicana", &["regional mexican"]),
    ("corridos", &["regional mexican"]),
    ("corrido", &["regional mexican"]),
    ("corridos tumbados", &["regional mexican"]),
    ("corridos belicos", &["regional mexican"]),
    ("banda", &["regional mexican"]),
    ("norteno", &["regional mexican"]),
    ("nortena", &["regional mexican"]),
    ("mariachi", &["regional mexican"]),
    ("ranchera", &["regional mexican"]),
    ("sierreno", &["regional mexican"]),
    ("grupera", &["regional mexican"]),
    ("bolero", &["bolero"]),
    ("boleros", &["bolero"]),
    ("tango", &["tango"]),
    ("flamenco", &["flamenco"]),
    ("rumba", &["flamenco"]),
    ("copla", &["flamenco"]),
    ("rock en espanol", &["rock en espanol"]),
    ("rock latino", &["rock en espanol"]),
    ("spanish rock", &["rock en espanol"]),
    ("rock nacional", &["rock en espanol"]),
    ("rock argentino", &["rock en espanol"]),
    ("rock mexicano", &["rock en espanol"]),
    ("bossa nova", &["bossa nova"]),
    ("mpb", &["bossa nova"]),
    ("samba", &["samba"]),
    ("pagode", &["samba"]),
    ("sertanejo", &["sertanejo"]),
    ("brazilian funk", &["brazilian funk"]),
    ("funk carioca", &["brazilian funk"]),
    ("baile funk", &["brazilian funk"]),
    ("funk brasileiro", &["brazilian funk"]),
];

/// Single words inside longer tags ("dark synth metal" -> metal).
const WORDS: &[(&str, &str)] = &[
    ("pop", "pop"),
    ("rock", "rock"),
    ("metal", "metal"),
    ("punk", "punk"),
    ("emo", "emo"),
    ("grunge", "grunge"),
    ("indie", "indie"),
    ("alternative", "alternative"),
    ("rap", "hip hop"),
    ("trap", "trap"),
    ("drill", "drill"),
    ("soul", "soul"),
    ("funk", "funk"),
    ("disco", "disco"),
    ("electronic", "electronic"),
    ("electronica", "electronic"),
    ("edm", "edm"),
    ("house", "house"),
    ("techno", "techno"),
    ("trance", "trance"),
    ("dubstep", "dubstep"),
    ("ambient", "ambient"),
    ("jazz", "jazz"),
    ("blues", "blues"),
    ("classical", "classical"),
    ("folk", "folk"),
    ("country", "country"),
    ("acoustic", "acoustic"),
    ("instrumental", "instrumental"),
    ("gospel", "gospel"),
    ("christian", "christian"),
    ("experimental", "experimental"),
    ("reggae", "reggae"),
    ("dancehall", "dancehall"),
    ("afrobeats", "afrobeats"),
    ("latin", "latin"),
    ("latino", "latin"),
    ("reggaeton", "reggaeton"),
    ("regueton", "reggaeton"),
    ("dembow", "dembow"),
    ("bachata", "bachata"),
    ("salsa", "salsa"),
    ("merengue", "merengue"),
    ("cumbia", "cumbia"),
    ("vallenato", "vallenato"),
    ("corridos", "regional mexican"),
    ("bolero", "bolero"),
    ("tango", "tango"),
    ("flamenco", "flamenco"),
    ("samba", "samba"),
    ("sertanejo", "sertanejo"),
    ("kpop", "k-pop"),
    ("jpop", "j-pop"),
    ("anime", "anime"),
    ("soundtrack", "soundtrack"),
];

/// Multi-word phrases searched inside longer tags.
const PHRASES: &[(&str, &str)] = &[
    ("hip hop", "hip hop"),
    ("r b", "r&b"),
    ("drum and bass", "drum and bass"),
    ("drum n bass", "drum and bass"),
    ("lo fi", "lo-fi"),
    ("bossa nova", "bossa nova"),
    ("hard rock", "hard rock"),
    ("classic rock", "classic rock"),
    ("k pop", "k-pop"),
    ("j pop", "j-pop"),
];

/// Exact normalized tags that are never genres.
const JUNK: &[&str] = &[
    "seen live",
    "favorites",
    "favourites",
    "favorite",
    "favourite",
    "my favorite",
    "my favorites",
    "love",
    "loved",
    "beautiful",
    "awesome",
    "cool",
    "amazing",
    "good",
    "great",
    "best",
    "female vocalists",
    "male vocalists",
    "female vocalist",
    "male vocalist",
    "female vocals",
    "male vocals",
    "female",
    "male",
    "vocal",
    "vocals",
    "spotify",
    "youtube",
    "albums i own",
    "check out",
    "catchy",
    "sexy",
    "party",
    "summer",
    "sad",
    "happy",
    "romantic",
    "romantico",
    "mellow",
    "chill vibes",
    "fun",
    "epic",
    "genius",
    "classic",
    "hits",
    "top 40",
    "billboard",
    "viral",
    "tiktok",
    "single",
    "album",
    "cover",
    "covers",
    "remix",
    "live",
    "en vivo",
    "seen in concert",
    "american",
    "usa",
    "us",
    "british",
    "uk",
    "english",
    "spanish",
    "espanol",
    "mexican",
    "mexico",
    "mexicano",
    "puerto rico",
    "puerto rican",
    "puertorriqueno",
    "colombian",
    "colombia",
    "colombiano",
    "argentina",
    "argentinian",
    "argentino",
    "chile",
    "chilean",
    "chileno",
    "peru",
    "peruvian",
    "venezuela",
    "venezuelan",
    "dominican",
    "dominicana",
    "cuba",
    "cuban",
    "brazil",
    "brazilian",
    "brasil",
    "korean",
    "korea",
    "japanese",
    "japan",
    "french",
    "german",
    "italian",
    "canadian",
    "swedish",
    "australian",
    "irish",
    "scottish",
    "spain",
    "espana",
    "all",
    "music",
    "musica",
    "songs",
    "song",
    "artist",
    "band",
    "bands",
    "unknown",
    "other",
    "misc",
];

/// Substrings that mark junk anywhere in a tag.
const JUNK_PARTS: &[&str] = &[
    "seen live",
    "favorite",
    "favourite",
    "listeners",
    "albums i own",
    "best of",
    "my ",
    "check out",
    "spotify",
];

/// Extra words that only make sense in a free-text prompt ("algo chill para estudiar").
const PROMPT_HINTS: &[(&str, &[&str])] = &[
    ("study", &["lo-fi", "ambient", "classical"]),
    ("studying", &["lo-fi", "ambient", "classical"]),
    ("estudiar", &["lo-fi", "ambient", "classical"]),
    ("focus", &["lo-fi", "ambient"]),
    ("concentrarme", &["lo-fi", "ambient"]),
    ("concentrar", &["lo-fi", "ambient"]),
    ("relax", &["chillout", "acoustic"]),
    ("relajar", &["chillout", "acoustic"]),
    ("relajarme", &["chillout", "acoustic"]),
    ("tranquilo", &["chillout", "acoustic"]),
    ("tranquila", &["chillout", "acoustic"]),
    ("sleep", &["ambient", "classical"]),
    ("dormir", &["ambient", "classical"]),
    ("party", &["edm", "pop", "reggaeton"]),
    ("fiesta", &["reggaeton", "latin pop", "edm"]),
    ("bailar", &["reggaeton", "salsa", "edm"]),
    ("perrear", &["reggaeton", "dembow"]),
    ("workout", &["edm", "hip hop"]),
    ("gym", &["edm", "hip hop"]),
    ("entrenar", &["edm", "hip hop"]),
    ("romantic", &["r&b", "latin pop"]),
    ("romantica", &["latin pop", "bolero"]),
    ("romanticas", &["latin pop", "bolero"]),
    ("sad", &["indie", "acoustic"]),
    ("triste", &["indie", "acoustic", "latin pop"]),
    ("tristes", &["indie", "acoustic", "latin pop"]),
    ("chill", &["chillout", "lo-fi"]),
    ("mexicana", &["regional mexican"]),
    ("mexicanas", &["regional mexican"]),
    ("rock", &["rock"]),
];

struct Tables {
    synonyms: HashMap<&'static str, &'static [&'static str]>,
    words: HashMap<&'static str, &'static str>,
    junk: std::collections::HashSet<&'static str>,
}

fn tables() -> &'static Tables {
    static T: OnceLock<Tables> = OnceLock::new();
    T.get_or_init(|| {
        let mut synonyms: HashMap<&'static str, &'static [&'static str]> = SYNONYMS.iter().copied().collect();
        // Canonical names map to themselves in their normalized spelling ("r&b" -> "r b").
        for g in GENRES {
            let n: &'static str = Box::leak(normalize(g).into_boxed_str());
            synonyms.entry(n).or_insert_with(|| std::slice::from_ref(g));
        }
        Tables {
            synonyms,
            words: WORDS.iter().copied().collect(),
            junk: JUNK.iter().copied().collect(),
        }
    })
}

/// The canonical spelling if `name` is (any spelling of) a canonical genre.
pub fn canonical(name: &str) -> Option<&'static str> {
    let n = normalize(name);
    GENRES.iter().copied().find(|g| *g == name || normalize(g) == n)
}

fn is_year_like(n: &str) -> bool {
    // "1999", "80s", "2010s", "90 s", "00s"
    let compact: String = n.chars().filter(|c| !c.is_whitespace()).collect();
    let digits = compact.trim_end_matches('s');
    !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
}

/// Normalize one raw tag.
pub fn normalize_tag(raw: &str) -> TagMatch {
    let n = normalize(raw);
    if n.is_empty() || n.len() > 40 {
        return TagMatch::Junk;
    }
    let t = tables();
    if let Some(g) = t.synonyms.get(n.as_str()) {
        return TagMatch::Genres(g.to_vec());
    }
    if t.junk.contains(n.as_str()) || is_year_like(&n) || JUNK_PARTS.iter().any(|p| n.contains(p)) {
        return TagMatch::Junk;
    }
    let mut out: Vec<&'static str> = Vec::new();
    let mut push = |g: &'static str| {
        if !out.contains(&g) {
            out.push(g);
        }
    };
    for (phrase, g) in PHRASES {
        if contains_phrase(&n, phrase) {
            push(g);
        }
    }
    for word in n.split(' ') {
        if let Some(g) = t.words.get(word) {
            push(g);
        }
    }
    if out.is_empty() {
        // Single words with digits or very long phrases are rarely genres.
        if n.chars().any(|c| c.is_ascii_digit()) || n.split(' ').count() > 4 {
            return TagMatch::Junk;
        }
        return TagMatch::Unknown;
    }
    out.truncate(3);
    TagMatch::Genres(out)
}

/// (canonical genre, weight 0..1).
pub type GenreWeights = Vec<(&'static str, f64)>;

/// Raw (tag, weight 0..1) pairs -> canonical (genre, weight 0..1), heaviest first, top `max`.
/// The heaviest genre is scaled to 1.0. Unknown tags are returned separately (with their weight).
pub fn normalize_weighted(raw: &[(String, f64)], max: usize) -> (GenreWeights, Vec<(String, f64)>) {
    let mut acc: HashMap<&'static str, f64> = HashMap::new();
    let mut unknown = Vec::new();
    for (tag, w) in raw {
        let w = w.clamp(0.0, 1.0);
        if w <= 0.0 {
            continue;
        }
        match normalize_tag(tag) {
            TagMatch::Genres(gs) => {
                for g in gs {
                    let e = acc.entry(g).or_insert(0.0);
                    *e = e.max(w);
                }
            }
            TagMatch::Unknown => unknown.push((normalize(tag), w)),
            TagMatch::Junk => {}
        }
    }
    (finish(acc, max), unknown)
}

/// Sort, cut to `max`, scale the top to 1.0 and drop very light entries.
pub fn finish(acc: HashMap<&'static str, f64>, max: usize) -> Vec<(&'static str, f64)> {
    let mut v: Vec<(&'static str, f64)> = acc.into_iter().filter(|(_, w)| *w > 0.0).collect();
    v.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(b.0)));
    v.truncate(max);
    let top = v.first().map(|x| x.1).unwrap_or(1.0);
    v.into_iter()
        .map(|(g, w)| (g, w / top))
        .filter(|(_, w)| *w >= 0.1)
        .collect()
}

/// Last.fm tag to query for a canonical genre (`tag.getTopTracks`).
pub fn lastfm_tag(genre: &str) -> &str {
    match genre {
        "rock en espanol" => "rock en español",
        "regional mexican" => "regional mexicano",
        "latin trap" => "trap latino",
        "r&b" => "rnb",
        "brazilian funk" => "funk carioca",
        "children" => "children's music",
        "edm" => "electronic dance music",
        other => other,
    }
}

/// Genres mentioned in a free-text prompt (keywords and mood hints), in order of appearance.
pub fn genres_in_text(text: &str) -> Vec<&'static str> {
    let n = normalize(text);
    let mut out: Vec<&'static str> = Vec::new();
    let mut push = |g: &'static str| {
        if !out.contains(&g) {
            out.push(g);
        }
    };
    let t = tables();
    // Multi-word synonyms first ("rock en espanol", "regional mexicano", "hip hop").
    for (syn, gs) in SYNONYMS.iter().filter(|(s, _)| s.contains(' ')) {
        if contains_phrase(&n, syn) {
            gs.iter().for_each(|g| push(g));
        }
    }
    for g in GENRES.iter().filter(|g| normalize(g).contains(' ')) {
        if contains_phrase(&n, &normalize(g)) {
            push(g);
        }
    }
    for word in n.split(' ') {
        if let Some(gs) = t.synonyms.get(word) {
            gs.iter().for_each(|g| push(g));
        } else if let Some(g) = t.words.get(word) {
            push(g);
        }
        if let Some((_, gs)) = PROMPT_HINTS.iter().find(|(w, _)| *w == word) {
            gs.iter().for_each(|g| push(g));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(raw: &str) -> Vec<&'static str> {
        match normalize_tag(raw) {
            TagMatch::Genres(v) => v,
            other => panic!("{raw}: {other:?}"),
        }
    }

    #[test]
    fn taxonomy_is_about_sixty_and_unique() {
        assert!((55..=75).contains(&GENRES.len()), "{}", GENRES.len());
        let mut s = GENRES.to_vec();
        s.sort();
        s.dedup();
        assert_eq!(s.len(), GENRES.len());
        // Every synonym target is canonical.
        for (_, gs) in SYNONYMS {
            for x in *gs {
                assert!(GENRES.contains(x), "{x}");
            }
        }
        for (_, x) in WORDS.iter().chain(PHRASES) {
            assert!(GENRES.contains(x), "{x}");
        }
        for (_, gs) in PROMPT_HINTS {
            for x in *gs {
                assert!(GENRES.contains(x), "{x}");
            }
        }
    }

    #[test]
    fn hip_hop_spellings_merge() {
        for raw in [
            "hip hop",
            "Hip-Hop",
            "hiphop",
            "RAP",
            "Hip Hop/Rap",
            "gangsta rap",
            "east coast rap",
        ] {
            assert_eq!(g(raw), vec!["hip hop"], "{raw}");
        }
        assert_eq!(g("R&B"), vec!["r&b"]);
        assert_eq!(g("rnb"), vec!["r&b"]);
        assert_eq!(g("Drum & Bass"), vec!["drum and bass"]);
        assert_eq!(g("lo-fi"), vec!["lo-fi"]);
        assert_eq!(g("Lo-Fi Hip Hop"), vec!["lo-fi"]);
    }

    #[test]
    fn compound_tags() {
        assert_eq!(g("indie rock"), vec!["indie", "rock"]);
        assert_eq!(g("heavy metal"), vec!["metal"]);
        assert_eq!(g("deep house"), vec!["house"]);
        assert_eq!(g("Alternative Rock"), vec!["alternative", "rock"]);
        assert_eq!(g("dream pop"), vec!["pop"]);
    }

    #[test]
    fn latin_and_spanish_tags() {
        assert_eq!(g("Reggaetón"), vec!["reggaeton"]);
        assert_eq!(g("reguetón"), vec!["reggaeton"]);
        assert_eq!(g("perreo"), vec!["reggaeton"]);
        assert_eq!(g("urbano latino"), vec!["reggaeton"]);
        assert_eq!(g("Trap Latino"), vec!["latin trap"]);
        assert_eq!(g("latin trap"), vec!["latin trap"]);
        assert_eq!(g("Pop Latino"), vec!["latin pop"]);
        assert_eq!(g("Música Mexicana"), vec!["regional mexican"]);
        assert_eq!(g("corridos tumbados"), vec!["regional mexican"]);
        assert_eq!(g("Norteño"), vec!["regional mexican"]);
        assert_eq!(g("Rock en Español"), vec!["rock en espanol"]);
        assert_eq!(g("rock latino"), vec!["rock en espanol"]);
        assert_eq!(g("Bachata"), vec!["bachata"]);
        assert_eq!(g("salsa romántica"), vec!["salsa"]);
        assert_eq!(g("cumbia villera"), vec!["cumbia"]);
        assert_eq!(g("dembow"), vec!["dembow"]);
        assert_eq!(g("Funk Carioca"), vec!["brazilian funk"]);
        assert_eq!(g("Clásica"), vec!["classical"]);
        assert_eq!(g("latin hip hop"), vec!["hip hop", "latin"]);
    }

    #[test]
    fn junk_is_dropped() {
        for raw in [
            "seen live",
            "Favorites",
            "my favourite songs",
            "female vocalists",
            "2010s",
            "80s",
            "80's",
            "1999",
            "spanish",
            "Puerto Rico",
            "under 2000 listeners",
            "albums I own",
            "love",
            "",
        ] {
            assert_eq!(normalize_tag(raw), TagMatch::Junk, "{raw}");
        }
        assert_eq!(normalize_tag("vaporwave"), TagMatch::Unknown);
    }

    #[test]
    fn weighted_normalization() {
        let raw: Vec<(String, f64)> = [
            ("reggaeton", 0.5),
            ("Reggaetón", 0.4),
            ("latin", 0.25),
            ("seen live", 1.0),
            ("zzzgenre", 0.3),
        ]
        .into_iter()
        .map(|(t, w)| (t.to_string(), w))
        .collect();
        let (genres, unknown) = normalize_weighted(&raw, 5);
        assert_eq!(genres, vec![("reggaeton", 1.0), ("latin", 0.5)]);
        assert_eq!(unknown, vec![("zzzgenre".to_string(), 0.3)]);
    }

    #[test]
    fn prompt_genres() {
        let p = genres_in_text("algo chill para estudiar");
        assert!(p.contains(&"chillout") && p.contains(&"lo-fi"), "{p:?}");
        let p = genres_in_text("reggaetón viejo y un poco de rock en español");
        assert!(p.contains(&"reggaeton") && p.contains(&"rock en espanol"), "{p:?}");
        assert!(genres_in_text("hola").is_empty());
        assert_eq!(canonical("R&B"), Some("r&b"));
        assert_eq!(canonical("Latin Pop"), Some("latin pop"));
        assert_eq!(canonical("nope"), None);
    }
}
