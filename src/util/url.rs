//! URL helpers. Always percent-encode anything derived from filenames or user text before
//! building URL paths or query strings. Never interpolate such text directly.

use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};

/// Characters to encode in URL path segments. Includes space and path-breaking characters
/// while leaving safe characters (`a-z`, `A-Z`, `0-9`, `-`, `_`, `.`, `~`) alone.
const PATH_SEGMENT: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}')
    .add(b'%')
    .add(b'/')
    .add(b'\\')
    .add(b'&')
    .add(b'+')
    .add(b'=');

pub fn encode_path_segment(s: &str) -> String {
    utf8_percent_encode(s, PATH_SEGMENT).to_string()
}

/// Percent-encode a query-string value.
pub fn encode_query_value(s: &str) -> String {
    urlencoding::encode(s).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_spaces_and_cjk() {
        let seg = encode_path_segment("hello world 漢字.mp4");
        // Spaces become %20, CJK is percent-encoded UTF-8.
        assert!(seg.contains("%20"));
        assert!(!seg.contains(' '));
        assert!(!seg.contains('漢'));
    }

    #[test]
    fn leaves_safe_chars_alone() {
        let seg = encode_path_segment("file-name_v1.2.mp4");
        assert_eq!(seg, "file-name_v1.2.mp4");
    }

    #[test]
    fn query_value_encodes_ampersand() {
        assert_eq!(encode_query_value("a&b"), "a%26b");
    }
}
