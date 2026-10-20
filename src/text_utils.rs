use std::borrow::Cow;

use crate::models::Track;

/// The usual filesystem limit for one path component.
pub const MAX_COMPONENT_BYTES: usize = 255;
/// Leaves room for the longest extension (`.flac.part`) and an inferred `0000. ` prefix.
const MAX_TRACK_STEM_BYTES: usize = MAX_COMPONENT_BYTES - ".flac.part".len() - "0000. ".len();

/// Replaces characters that are reserved on common filesystems or are control characters.
pub fn sanitize_file_name(name: &str) -> String {
    name.trim_ascii()
        .chars()
        .map(|char| {
            if char.is_control()
                || matches!(char, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
            {
                '_'
            } else {
                char
            }
        })
        .collect()
}

/// Makes a sanitized name usable as a whole path component: never empty, `.` or `..`,
/// and at most `max_bytes` long.
pub fn path_component(mut name: String, max_bytes: usize) -> String {
    if name.len() > max_bytes {
        let mut end = max_bytes;
        while !name.is_char_boundary(end) {
            end -= 1;
        }
        name.truncate(end);
    }
    // Windows silently drops trailing dots and spaces; elsewhere they are kept so
    // existing libraries keep matching.
    if cfg!(windows) {
        name.truncate(name.trim_end_matches(['.', ' ']).len());
    }
    if matches!(name.as_str(), "" | "." | "..") {
        "_".into()
    } else {
        name
    }
}

pub fn parse_enclosed_value<'a>(
    start_marker: &str,
    end_marker: &str,
    text: &'a str,
) -> Option<&'a str> {
    let start = text.find(start_marker)? + start_marker.len();
    let end = text[start..].find(end_marker)? + start;
    Some(&text[start..end])
}

pub fn format_track_stem(
    track: &Track,
    track_number: Option<u32>,
    track_count: u32,
    is_grouped_single: bool,
) -> String {
    let track_number_and_artist = if is_grouped_single {
        Cow::Borrowed("")
    } else {
        let track_number = track_number.map_or_else(String::new, |number| {
            crate::numbering::format_prefix(number, track_count)
        });

        let artist = if let [artist, ..] = track.artists.as_slice() {
            format!("{} - ", sanitize_file_name(&artist.name))
        } else {
            String::new()
        };

        Cow::Owned(track_number + &artist)
    };

    path_component(
        format!(
            "{track_number_and_artist}{}",
            sanitize_file_name(&track.title)
        ),
        MAX_TRACK_STEM_BYTES,
    )
}

#[cfg(test)]
mod tests {
    use super::{parse_enclosed_value, path_component, sanitize_file_name};

    #[test]
    fn names_cannot_escape_or_exceed_their_directory() {
        for name in ["", "  ", ".", ".."] {
            assert_eq!(path_component(sanitize_file_name(name), 255), "_");
        }
        assert_eq!(sanitize_file_name("../a\tb\nc"), ".._a_b_c");
        let long = path_component("é".repeat(200), 255);
        assert_eq!(long.len(), 254);
        assert!(long.chars().all(|char| char == 'é'));
    }

    #[test]
    fn changed_or_truncated_pages_do_not_panic() {
        assert_eq!(parse_enclosed_value("BEGIN", "END", "challenge page"), None);
        assert_eq!(parse_enclosed_value("BEGIN", "END", "BEGINpartial"), None);
        assert_eq!(
            parse_enclosed_value("BEGIN", "END", "xBEGIN[1]ENDx"),
            Some("[1]")
        );
    }
}
