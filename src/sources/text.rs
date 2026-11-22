//! Small text helpers shared by the parsers and the matcher.

use unicode_normalization::{char::is_combining_mark, UnicodeNormalization};

/// Lowercase, strip accents, turn every non-alphanumeric run into one space.
pub fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut space = true;
    for c in s.nfkd().filter(|c| !is_combining_mark(*c)) {
        let c = match c {
            '&' => {
                // "&" and "and"/"y" are interchangeable in artist names; treat as a separator.
                ' '
            }
            'ß' => 's',
            'ø' | 'Ø' => 'o',
            'đ' | 'Đ' => 'd',
            'ł' | 'Ł' => 'l',
            _ => c,
        };
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
            space = false;
        } else if !space {
            out.push(' ');
            space = true;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

pub fn tokens(s: &str) -> Vec<String> {
    normalize(s)
        .split(' ')
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Whole-word (or whole-phrase) containment on normalized text.
pub fn contains_phrase(normalized_haystack: &str, normalized_phrase: &str) -> bool {
    if normalized_phrase.is_empty() {
        return false;
    }
    let h = format!(" {normalized_haystack} ");
    h.contains(&format!(" {normalized_phrase} "))
}

/// "3:45", "1:02:03" -> milliseconds.
pub fn parse_clock(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let mut total: u64 = 0;
    let mut parts = 0;
    for p in s.split(':') {
        if p.is_empty() || (p.len() > 2 && parts > 0) || !p.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        total = total.checked_mul(60)?.checked_add(p.parse().ok()?)?;
        parts += 1;
    }
    (2..=3).contains(&parts).then_some(total * 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_accents_and_punctuation() {
        assert_eq!(normalize("Canción Ñandú - (Remix)!!"), "cancion nandu remix");
        assert_eq!(normalize("  Beyoncé & JAY-Z "), "beyonce jay z");
        assert_eq!(normalize("Mötley Crüe"), "motley crue");
        assert_eq!(normalize("ＡＢＣ"), "abc");
        assert_eq!(normalize(""), "");
    }

    #[test]
    fn phrase_matching_is_word_based() {
        assert!(contains_phrase("song live at wembley", "live"));
        assert!(!contains_phrase("oliver", "live"));
        assert!(contains_phrase("track sped up version", "sped up"));
    }

    #[test]
    fn clock() {
        assert_eq!(parse_clock("3:45"), Some(225_000));
        assert_eq!(parse_clock("1:02:03"), Some(3_723_000));
        assert_eq!(parse_clock("0:07"), Some(7_000));
        assert_eq!(parse_clock("45"), None);
        assert_eq!(parse_clock("Song"), None);
        assert_eq!(parse_clock("1:2x"), None);
    }
}
