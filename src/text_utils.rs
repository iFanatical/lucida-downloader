use std::borrow::Cow;

use crate::models::Track;

pub fn sanitize_file_name(name: &str) -> String {
    name.trim_ascii()
        .replace(['\\', '/', ':', '*', '?', '"', '<', '>', '|'], "_")
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

    format!(
        "{track_number_and_artist}{}",
        sanitize_file_name(&track.title)
    )
}

#[cfg(test)]
mod tests {
    use super::parse_enclosed_value;
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
