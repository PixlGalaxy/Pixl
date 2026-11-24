//! Cleaning of untrusted one-line text (track titles, artist and playlist names from the sources).

/// One clean line: control characters (newlines and tabs included) and invisible bidi / zero-width
/// formatting characters become spaces, runs of spaces collapse into one, and the ends are trimmed.
/// Safe for titles and labels; markup escaping is the renderer's job.
pub fn sanitize_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut space = false;
    for c in s.chars() {
        let c = if c.is_control() || is_invisible_format(c) || c.is_whitespace() {
            ' '
        } else {
            c
        };
        if c == ' ' {
            if !space && !out.is_empty() {
                out.push(' ');
            }
            space = true;
        } else {
            out.push(c);
            space = false;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

/// Bidi overrides / isolates / marks and zero-width spaces: they can reorder or hide what a viewer
/// sees (joiners stay, emoji sequences need them).
fn is_invisible_format(c: char) -> bool {
    matches!(
        c,
        '\u{200B}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_clean_line() {
        assert_eq!(sanitize_text("  Song\nTitle\r\n\t(Live)  "), "Song Title (Live)");
        assert_eq!(sanitize_text("a\u{202E}b\u{200B}c"), "a b c");
        assert_eq!(sanitize_text("a\u{0}\u{7}b"), "a b");
        assert_eq!(sanitize_text("Beyoncé – Halo"), "Beyoncé – Halo");
        assert_eq!(sanitize_text("\n\n"), "");
        assert_eq!(sanitize_text("<@123> & [x](y)"), "<@123> & [x](y)", "markup is left to the renderer");
    }
}
