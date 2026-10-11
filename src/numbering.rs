//! Filename numbering for standalone retries, using the completed audio's tags.
use std::collections::HashSet;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::Value;
use tokio::{fs, process::Command, time};

static FFPROBE_AVAILABLE: AtomicBool = AtomicBool::new(false);

/// Checks once at startup whether ffprobe can be run for standalone-track numbering.
pub async fn detect_ffprobe() -> bool {
    let available = time::timeout(
        Duration::from_secs(5),
        Command::new("ffprobe")
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .status(),
    )
    .await
    .is_ok_and(|status| status.is_ok_and(|status| status.success()));
    FFPROBE_AVAILABLE.store(available, Ordering::Relaxed);
    available
}

pub fn ffprobe_available() -> bool {
    FFPROBE_AVAILABLE.load(Ordering::Relaxed)
}

#[derive(Clone)]
pub struct DownloadName {
    pub stem: String,
    pub infer_number: bool,
    pub track_count: u32,
    pub force: bool,
}

#[derive(Debug, Default)]
struct Tags {
    track: u32,
    total: u32,
    disc: u32,
}

fn positive(value: &str) -> Option<u32> {
    value.trim().parse::<u32>().ok().filter(|value| *value > 0)
}

fn parse_tags(value: &Value) -> Result<Tags, String> {
    let mut tags = std::collections::HashMap::new();
    // Opus commonly stores tags on its audio stream; FLAC/MP3 on the container.
    if let Some(streams) = value["streams"].as_array() {
        for stream in streams {
            if let Some(map) = stream["tags"].as_object() {
                for (key, value) in map {
                    if let Some(value) = value.as_str() {
                        tags.insert(key.to_ascii_lowercase(), value.to_owned());
                    }
                }
            }
        }
    }
    if let Some(map) = value["format"]["tags"].as_object() {
        for (key, value) in map {
            if let Some(value) = value.as_str() {
                tags.insert(key.to_ascii_lowercase(), value.to_owned());
            }
        }
    }
    let raw = tags
        .get("track")
        .or_else(|| tags.get("tracknumber"))
        .ok_or("Audio has no track-number tag")?;
    let mut parts = raw.split('/');
    let track = positive(parts.next().unwrap_or_default()).ok_or("Invalid track-number tag")?;
    let total = parts
        .next()
        .and_then(positive)
        .or_else(|| {
            tags.get("tracktotal")
                .or_else(|| tags.get("totaltracks"))
                .and_then(|v| positive(v))
        })
        .unwrap_or(track);
    let disc = tags
        .get("disc")
        .or_else(|| tags.get("discnumber"))
        .and_then(|value| value.split('/').next())
        .and_then(positive)
        .unwrap_or(1);
    Ok(Tags { track, total, disc })
}

async fn read_tags(path: &Path) -> Result<Tags, String> {
    let output = time::timeout(
        Duration::from_secs(15),
        Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                "a:0",
                "-show_entries",
                "format_tags:stream_tags",
                "-of",
                "json",
            ])
            .arg(path)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| "ffprobe timed out".to_owned())?
    .map_err(|error| format!("Cannot run ffprobe (needed for standalone numbering): {error}"))?;
    if !output.status.success() {
        return Err("ffprobe could not read the audio tags".into());
    }
    parse_tags(&serde_json::from_slice(&output.stdout).map_err(|error| error.to_string())?)
}

pub fn numbered_suffix(stem: &str) -> Option<(u32, &str)> {
    let (number, suffix) = stem.split_once(". ")?;
    positive(number).map(|number| (number, suffix))
}

pub fn format_prefix(number: u32, count: u32) -> String {
    let width = count.max(number).max(1).ilog10() as usize + 1;
    format!("{number:0width$}. ")
}

/// Infer disc offsets only from existing numbered tracks on the same disc.
/// This preserves the full-album downloader's flattened numbering without guessing.
pub async fn standalone_stem(
    path: &Path,
    directory: &Path,
    name: &DownloadName,
) -> Result<String, String> {
    let tags = read_tags(path).await?;
    let mut number = tags.track;
    {
        let mut offsets = HashSet::new();
        let mut entries = fs::read_dir(directory)
            .await
            .map_err(|error| error.to_string())?;
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|error| error.to_string())?
        {
            let path = entry.path();
            if !path
                .extension()
                .and_then(|v| v.to_str())
                .is_some_and(|ext| ["flac", "mp3", "m4a", "opus"].contains(&ext))
            {
                continue;
            }
            let Some((prefix, _)) = path
                .file_stem()
                .and_then(|v| v.to_str())
                .and_then(numbered_suffix)
            else {
                continue;
            };
            if let Ok(sibling) = read_tags(&path).await
                && sibling.disc == tags.disc
                && let Some(offset) = prefix.checked_sub(sibling.track)
            {
                offsets.insert(offset);
            }
        }
        if offsets.len() > 1 || (tags.disc > 1 && offsets.is_empty()) {
            return Err("Cannot establish a unique album-wide track number for this disc from existing tracks".into());
        }
        number = number
            .checked_add(offsets.iter().next().copied().unwrap_or(0))
            .ok_or("Track number overflow")?;
    }
    Ok(format!(
        "{}{}",
        format_prefix(number, name.track_count.max(tags.total)),
        name.stem
    ))
}

/// Use the same full-album enumeration as the normal downloader, matching IDs/URLs,
/// never titles (which can repeat on deluxe and live releases).
pub fn album_position(info: &crate::models::Info, track_url: &str) -> Option<(u32, u32)> {
    let crate::models::Info::Album {
        tracks,
        track_count,
        ..
    } = info
    else {
        return None;
    };
    let key = track_key(track_url)?;
    let mut matches = tracks
        .iter()
        .enumerate()
        .filter(|(_, track)| track_key(&track.url).as_ref() == Some(&key));
    let (index, _) = matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    Some((u32::try_from(index).ok()?.checked_add(1)?, *track_count))
}

fn track_key(raw: &str) -> Option<String> {
    let mut url = reqwest::Url::parse(raw).ok()?;
    if url
        .host_str()
        .is_some_and(|host| host.starts_with("music.amazon."))
    {
        let id = url
            .query_pairs()
            .find(|(key, _)| key.eq_ignore_ascii_case("trackAsin"))
            .map(|(_, id)| id.into_owned())
            .or_else(|| {
                url.path()
                    .strip_prefix("/tracks/")
                    .and_then(|path| path.split('/').next())
                    .map(str::to_owned)
            });
        if let Some(id) = id {
            return Some(format!("{}:{id}", url.origin().ascii_serialization()));
        }
    }
    url.set_fragment(None);
    Some(url.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn infers_number_from_completed_audio_and_multidisc_neighbors() {
        if !detect_ffprobe().await {
            eprintln!("skipping: ffprobe is not installed");
            return;
        }
        let directory =
            std::env::temp_dir().join(format!("lucida-numbering-test-{}", std::process::id()));
        fs::create_dir_all(&directory).await.unwrap();
        let part = directory.join("Artist - Song.flac.part");
        let name = DownloadName {
            stem: "Artist - Song".into(),
            infer_number: true,
            track_count: 12,
            force: false,
        };
        fs::write(
            &part,
            include_bytes!("../tests/fixtures/numbering-single.flac"),
        )
        .await
        .unwrap();
        assert_eq!(
            standalone_stem(&part, &directory, &name).await.unwrap(),
            "03. Artist - Song"
        );
        let disc2 = include_bytes!("../tests/fixtures/numbering-disc2.flac");
        fs::write(&part, disc2).await.unwrap();
        assert!(standalone_stem(&part, &directory, &name).await.is_err());
        fs::write(directory.join("12. Artist - Neighbor.flac"), disc2)
            .await
            .unwrap();
        assert_eq!(
            standalone_stem(&part, &directory, &name).await.unwrap(),
            "12. Artist - Song"
        );
        fs::write(
            directory.join("13. Artist - Conflicting neighbor.flac"),
            disc2,
        )
        .await
        .unwrap();
        assert!(standalone_stem(&part, &directory, &name).await.is_err());
        fs::remove_file(directory.join("12. Artist - Neighbor.flac"))
            .await
            .unwrap();
        fs::remove_file(directory.join("13. Artist - Conflicting neighbor.flac"))
            .await
            .unwrap();
        let first_disc = include_bytes!("../tests/fixtures/numbering-single.flac");
        fs::write(&part, first_disc).await.unwrap();
        fs::write(directory.join("03. Artist - First.flac"), first_disc)
            .await
            .unwrap();
        fs::write(directory.join("13. Artist - Second.flac"), first_disc)
            .await
            .unwrap();
        assert!(standalone_stem(&part, &directory, &name).await.is_err());
        fs::remove_dir_all(directory).await.unwrap();
    }

    #[test]
    fn parent_album_order_matches_track_identity_and_rejects_ambiguity() {
        let mut info: crate::models::Info = serde_json::from_value(serde_json::json!({
            "type":"album", "title":"Album", "coverArtwork":[], "artists":[], "trackCount":12,
            "releaseDate":"2020-01-01T00:00:00Z", "tracks":[
                {"title":"Repeated", "url":"https://music.amazon.com/tracks/B000000001", "artists":[], "csrf":"token"},
                {"title":"Repeated", "url":"https://music.amazon.com/tracks/B000000002", "artists":[], "csrf":"token"}
            ]
        })).unwrap();
        assert_eq!(
            album_position(
                &info,
                "https://music.amazon.com/albums/B000ALBUM1?trackAsin=B000000002"
            ),
            Some((2, 12))
        );
        assert_eq!(
            album_position(&info, "https://music.amazon.com/tracks/B000000003"),
            None
        );
        if let crate::models::Info::Album { tracks, .. } = &mut info {
            tracks[0].url = tracks[1].url.clone();
        }
        assert_eq!(
            album_position(&info, "https://music.amazon.com/tracks/B000000002"),
            None
        );
    }

    #[test]
    fn reads_container_and_stream_track_tags() {
        let tags = parse_tags(&serde_json::json!({"format":{"tags":{"track":"3/12"}}})).unwrap();
        assert_eq!((tags.track, tags.total, tags.disc), (3, 12, 1));
        let tags = parse_tags(&serde_json::json!({"streams":[{"tags":{"TRACKNUMBER":"7","TRACKTOTAL":"15","DISCNUMBER":"2/3"}}]})).unwrap();
        assert_eq!((tags.track, tags.total, tags.disc), (7, 15, 2));
        for raw in ["0", "invalid", "-2"] {
            assert!(parse_tags(&serde_json::json!({"format":{"tags":{"track":raw}}})).is_err());
        }
        assert!(parse_tags(&serde_json::json!({})).is_err());
    }
    #[test]
    fn matches_album_padding_and_existing_numbered_names() {
        assert_eq!(format_prefix(3, 12), "03. ");
        assert_eq!(format_prefix(3, 7), "3. ");
        assert_eq!(format_prefix(10, 0), "10. ");
        assert_eq!(
            numbered_suffix("03. Artist - Song"),
            Some((3, "Artist - Song"))
        );
        assert_eq!(numbered_suffix("3 Doors Down - Song"), None);
    }
}
