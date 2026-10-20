use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::future;
use reqwest::{Client, Response};
use tokio::fs::File;
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::{fs, time};
use tracing::Instrument;

use crate::models::{
    AlbumInfo, AlbumYear, DownloadConfig, Info, PageData, ResolveAlbumError, Service, SkipConfig,
    Track, TrackDownload,
};
use crate::numbering::{self, DownloadName};
use crate::report::{self, Item, Kind, Outcome};
use crate::{requests, text_utils, workers};

macro_rules! io_or_return {
    ($item:ident, $operation:expr) => {
        match $operation {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(
                    "file operation failed ({}): {error}",
                    stringify!($operation)
                );
                $item.fail(format!("file operation failed: {error}"));
                return;
            }
        }
    };
}

/// Removes a partial download unless it was promoted, including when the download is cancelled.
struct PartFile {
    path: PathBuf,
    keep: bool,
}

impl PartFile {
    const fn new(path: PathBuf) -> Self {
        Self { path, keep: false }
    }

    fn promoted(mut self) {
        self.keep = true;
    }
}

impl Drop for PartFile {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "this function is called from a single place"
)]
#[expect(
    clippy::too_many_lines,
    reason = "album resolution, standalone position recovery, and worker orchestration"
)]
pub async fn download_album(
    client: Client,
    url: &str,
    output_path: &Path,
    force_download: bool,
    group_singles: bool,
    album_year: Option<AlbumYear>,
    flatten_directories: bool,
    config: DownloadConfig,
    track_workers: usize,
    skip: SkipConfig,
    running: Arc<AtomicBool>,
) {
    let mut item = Item::new(url, Kind::Album);
    let page_data = match resolve_album(&client, url, &config, &running).await {
        Ok(page_data) => page_data,
        Err(error) => {
            item.fail(error);
            return;
        }
    };

    let recovered_position = if let Info::Track {
        url: track_url,
        album: Some(parent),
        ..
    } = &page_data.info
    {
        if let Some(parent_url) = parent.url.as_deref() {
            resolve_album(&client, parent_url, &config, &running)
                .await
                .ok()
                .and_then(|parent| numbering::album_position(&parent.info, track_url))
        } else {
            None
        }
    } else {
        None
    };
    let mut album = match AlbumInfo::new(page_data.info, page_data.token) {
        Ok(album) => album,
        Err(ResolveAlbumError::ArtistUrl { name }) => {
            tracing::error!("cannot download URL pointing to an artist ({name})");
            item.fail(format!(
                "URL points to an artist ({name}); use --discover-artist to list its albums"
            ));
            return;
        }
    };
    if let Some((number, count)) = recovered_position {
        album.tracks[0].0 = Some(number);
        album.track_count = count;
    }

    tracing::info!(
        "downloading album {} - {} with {} tracks",
        album.artist_name,
        album.title,
        album.track_count
    );

    let is_grouped_single = group_singles
        && album.track_count == 1
        && album
            .tracks
            .iter()
            .all(|track| track.1.title == album.title);

    let album_path = {
        let sanitized_artist_name = text_utils::sanitize_file_name(&album.artist_name);

        let album_directory = if is_grouped_single {
            "Singles".into()
        } else {
            let sanitized_album_title = text_utils::sanitize_file_name(&album.title);

            match (album.release_year, album_year) {
                (Some(release_year), Some(AlbumYear::Append)) => {
                    format!("{sanitized_album_title} ({release_year})")
                }
                (Some(release_year), Some(AlbumYear::Prepend)) => {
                    format!("({release_year}) {sanitized_album_title}")
                }
                _ => sanitized_album_title,
            }
        };

        let album_directory = if flatten_directories {
            vec![format!("{sanitized_artist_name} - {album_directory}")]
        } else {
            vec![sanitized_artist_name, album_directory]
        };

        let mut album_path = PathBuf::from(output_path);
        album_path.extend(
            album_directory
                .into_iter()
                .map(|name| text_utils::path_component(name, text_utils::MAX_COMPONENT_BYTES)),
        );

        album_path
    };

    item.describe(&album_path, "album");
    io_or_return!(item, fs::create_dir_all(&album_path).await);

    let tracks_len = album.tracks.len();
    let tracks = Arc::new(Mutex::new(album.tracks));
    let album_path = Arc::new(album_path);

    if !skip.tracks {
        let worker_count = track_workers.min(tracks_len);

        tracing::info!("spawning {worker_count} track workers");

        future::join_all((1..=worker_count).map(|track_worker| {
            workers::run_track_worker(
                client.clone(),
                page_data.original_service,
                tracks.clone(),
                album.track_count,
                is_grouped_single,
                page_data.token_expiry,
                force_download,
                config.clone(),
                album_path.clone(),
                running.clone(),
            )
            .instrument(tracing::info_span!("track", track_worker))
        }))
        .await;
    }

    if !running.load(Ordering::Relaxed) {
        return;
    }
    item.finish(Outcome::Downloaded);
    if skip.cover || is_grouped_single {
        return;
    }
    let Some(cover_artwork_url) = &album.cover_artwork_url else {
        tracing::warn!("{} has no cover artwork", album.title);
        report::warn(&album_path, "cover.jpg", "lucida listed no cover artwork");
        return;
    };

    let mut cover = Item::new(url, Kind::Cover);
    cover.describe(&album_path, "cover.jpg");
    let outcome = download_album_cover(
        client,
        &album.title,
        page_data.original_service,
        cover_artwork_url,
        force_download,
        &album_path,
        running,
    )
    .await;
    cover.finish(outcome);
}

async fn resolve_album(
    client: &Client,
    url: &str,
    config: &DownloadConfig,
    running: &Arc<AtomicBool>,
) -> Result<PageData, String> {
    tracing::info!("resolving album {url}");

    let mut last_error = String::from("album page did not contain usable metadata");
    for attempt in 0..3 {
        if !running.load(Ordering::Relaxed) {
            return Err("interrupted".into());
        }
        if attempt > 0 {
            time::sleep(Duration::from_secs(5)).await;
        }
        let html = requests::resolve_album(client, url, &config.country, running).await?;
        if let Some(error) = [
            "An error occured trying to process your request.",
            "Message: \"Cannot contact any valid server\"",
            "An error occurred. Had an issue getting that item, try again.",
        ]
        .into_iter()
        .find(|&error| html.contains(error))
        {
            tracing::warn!("HTML contains error: {error}");
            last_error = format!("lucida error: {error}");
            continue;
        }
        let Some(data) = text_utils::parse_enclosed_value(
            ",{\"type\":\"data\",\"data\":",
            ",\"uses\":{\"url\":1}}];\n",
            &html,
        ) else {
            tracing::warn!("missing album metadata for {url}");
            continue;
        };
        match json5::from_str(data) {
            Ok(data) => return Ok(data),
            Err(error) => {
                tracing::warn!("invalid album metadata for {url}: {error}");
            }
        }
    }
    tracing::error!("failed album {url} after 3 attempts");
    Err(format!("{last_error} (after 3 attempts)"))
}

#[expect(
    clippy::too_many_arguments,
    reason = "this function is called from a single place"
)]
pub async fn request_and_download_track(
    client: Client,
    service: Service,
    track: &Track,
    track_number: Option<u32>,
    track_count: u32,
    is_grouped_single: bool,
    token_expiry: u64,
    force_download: bool,
    config: &DownloadConfig,
    album_path: Arc<PathBuf>,
    running: Arc<AtomicBool>,
) {
    let mut item = Item::new(&track.url, Kind::Track);
    let file_stem =
        text_utils::format_track_stem(track, track_number, track_count, is_grouped_single);
    item.describe(&album_path, &file_stem);
    let infer_number = track_number.is_none() && !is_grouped_single;

    // HACK(jel): this seems to be the only way to detect tracks that are impossible
    // to download yet
    if matches!(service, Service::Qobuz if track.producers.is_none()) {
        tracing::warn!("skipping unavailable track {}", track.title);
        item.finish(Outcome::Unavailable);
        return;
    }

    if !force_download {
        let mut directory = io_or_return!(item, fs::read_dir(album_path.as_path()).await);

        while let Some(entry) = io_or_return!(item, directory.next_entry().await) {
            // A standalone track may already exist under the number inferred on a previous run.
            let matches = |stem: &str| {
                stem == file_stem
                    || (infer_number
                        && numbering::numbered_suffix(stem)
                            .is_some_and(|(_, rest)| rest == file_stem))
            };
            if io_or_return!(item, entry.file_type().await).is_file()
                && entry
                    .path()
                    .file_stem()
                    .is_some_and(|stem| stem.to_str().is_some_and(matches))
            {
                tracing::info!("track {} is already downloaded", track.title);
                item.finish(Outcome::Existing);
                return;
            }
        }
    }

    tracing::info!("downloading track {}", track.title);

    let outcome = request_track_download(
        client,
        track,
        DownloadName {
            stem: file_stem,
            infer_number,
            track_count,
            force: force_download,
        },
        token_expiry,
        config,
        album_path,
        running,
    )
    .await;
    item.finish(outcome);
}

#[expect(
    clippy::too_many_lines,
    reason = "bounded job lifecycle with status diagnostics"
)]
async fn request_track_download(
    client: Client,
    track: &Track,
    name: DownloadName,
    token_expiry: u64,
    config: &DownloadConfig,
    album_path: Arc<PathBuf>,
    running: Arc<AtomicBool>,
) -> Outcome {
    let interrupted = || Outcome::Failed("interrupted".into());
    let mut last_error = String::new();
    'request_track_download: for attempt in 0..3 {
        if !running.load(Ordering::Relaxed) {
            return interrupted();
        }
        if attempt > 0 {
            time::sleep(Duration::from_secs(5)).await;
        }
        let track_download =
            match requests::request_track_download(&client, track, token_expiry, config, &running)
                .await
            {
                Ok(job) => job,
                Err(requests::JobError::Unavailable) => return Outcome::Unavailable,
                Err(requests::JobError::Failed(error)) => {
                    last_error = error;
                    break;
                }
            };

        let mut last_status: Option<(String, String, Instant)> = None;
        let started = Instant::now();

        loop {
            if !running.load(Ordering::Relaxed) {
                return interrupted();
            }
            if started.elapsed() >= Duration::from_mins(3) {
                last_error = "lucida processing timed out after 3 minutes".into();
                continue 'request_track_download;
            }
            let Some(track_download) = time::timeout(
                Duration::from_mins(3).saturating_sub(started.elapsed()),
                requests::track_download_status(&client, &track_download, &running),
            )
            .await
            .ok()
            .flatten() else {
                if !running.load(Ordering::Relaxed) {
                    return interrupted();
                }

                last_error = "lost track of the lucida processing job".into();
                continue 'request_track_download;
            };

            if last_status.as_ref().is_none_or(|last_status| {
                (&track_download.status, &track_download.message)
                    != (&last_status.0, &last_status.1)
            }) {
                tracing::info!(
                    "new download status: {}: {}",
                    track_download.status,
                    track_download.message.replace("{item}", &track.title)
                );

                last_status = Some((
                    track_download.status.clone(),
                    track_download.message.clone(),
                    Instant::now(),
                ));
            } else if let Some(last_status) = last_status.as_ref()
                && last_status.2.elapsed() >= Duration::from_secs(30)
            {
                let is_running = running.load(Ordering::Relaxed);

                tracing::warn!(
                    "download status stuck for 30 seconds on {}: {}{}",
                    last_status.0,
                    last_status.1.replace("{item}", &track.title),
                    if is_running { ", retrying" } else { "" }
                );

                if !is_running {
                    return interrupted();
                }

                last_error = format!(
                    "lucida processing stuck on {}: {}",
                    last_status.0,
                    last_status.1.replace("{item}", &track.title)
                );
                continue 'request_track_download;
            }

            if requests::unavailable(&track_download.message)
                || track_download.status == "unavailable"
            {
                tracing::warn!("skipping unavailable track {}", track.title);
                return Outcome::Unavailable;
            }
            if matches!(
                track_download.status.as_str(),
                "error" | "failed" | "cancelled"
            ) {
                last_error = format!(
                    "lucida processing {}: {}",
                    track_download.status,
                    track_download.message.replace("{item}", &track.title)
                );
                continue 'request_track_download;
            }
            if track_download.status == "completed" {
                break;
            }

            time::sleep(Duration::from_secs(1)).await;
        }

        let outcome = download_track(
            client.clone(),
            track_download,
            album_path.clone(),
            name.clone(),
            running.clone(),
        )
        .await;
        match outcome {
            Outcome::Failed(error) => last_error = error,
            outcome => return outcome,
        }
    }
    tracing::error!(
        "failed track {} ({}) after retry budget exhausted",
        track.title,
        track.url
    );
    Outcome::Failed(last_error)
}

async fn download_track(
    client: Client,
    track_download: TrackDownload,
    album_path: Arc<PathBuf>,
    name: DownloadName,
    running: Arc<AtomicBool>,
) -> Outcome {
    let result: std::io::Result<Outcome> = async {
        let Some((mut response, mime_type)) =
            requests::download_track(&client, &track_download, &running).await
        else {
            if !running.load(Ordering::Relaxed) {
                return Ok(Outcome::Failed("interrupted".into()));
            }

            return Ok(Outcome::Failed("audio download request failed".into()));
        };

        let file_extension = match mime_type
            .split_once(';')
            .map_or(mime_type.as_str(), |(mime_type, _)| mime_type)
        {
            "audio/flac" => "flac",
            "audio/mpeg" => "mp3",
            "audio/mp4" => "m4a",
            "audio/opus" => "opus",
            _ => {
                tracing::warn!("unsupported mime type {mime_type}");
                return Ok(Outcome::Failed(format!("unsupported audio type {mime_type}")));
            }
        };

        let file_name = format!("{}.{file_extension}", name.stem);
        let part = PartFile::new(album_path.join(format!("{file_name}.part")));
        if let Err(error) = write_stream(&mut response, &part.path).await {
            tracing::warn!("audio transfer failed: {error}");
            return Ok(Outcome::Failed(format!("audio transfer failed: {error}")));
        }
        let final_stem = if !name.infer_number {
            name.stem.clone()
        } else if !numbering::ffprobe_available() {
            report::warn(&album_path, &file_name, "kept unnumbered: ffprobe is not installed");
            name.stem.clone()
        } else {
            match time::timeout(Duration::from_mins(1), numbering::standalone_stem(&part.path, &album_path, &name)).await.unwrap_or_else(|_| Err("Track-number inference timed out".into())) {
                Ok(stem) => stem,
                Err(error) => {
                    tracing::warn!("could not number standalone track: {error}; retaining the unnumbered filename");
                    report::warn(&album_path, &file_name, format!("kept unnumbered: {error}"));
                    name.stem.clone()
                }
            }
        };
        let final_path = album_path.join(format!("{final_stem}.{file_extension}"));
        if !name.force && final_path.exists() {
            tracing::warn!("numbered track already exists: {}", final_path.display());
            return Ok(Outcome::Existing);
        }
        fs::rename(&part.path, &final_path).await?;
        part.promoted();

        Ok(Outcome::Downloaded)
    }
    .await;
    match result {
        Ok(done) => done,
        Err(error) => {
            tracing::warn!("file operation failed: {error}");
            Outcome::Failed(format!("file operation failed: {error}"))
        }
    }
}

pub async fn download_album_cover(
    client: Client,
    title: &str,
    service: Service,
    url: &str,
    force_download: bool,
    album_path: &Path,
    running: Arc<AtomicBool>,
) -> Outcome {
    let cover_path = album_path.join("cover.jpg");

    if !force_download && cover_path.exists() {
        tracing::info!("{title} album cover is already downloaded");
        return Outcome::Existing;
    }

    tracing::info!("downloading {title} album cover");

    let url = match service {
        Service::Qobuz => url
            .strip_suffix(".jpg")
            .and_then(|url| url.rfind('_'))
            .map_or(Cow::Borrowed(url), |index| {
                Cow::Owned(format!("{}org.jpg", &url[..=index]))
            }),
        Service::Tidal | Service::Soundcloud | Service::Amazon => Cow::Borrowed(url),
    };

    let part_path = album_path.join("cover.jpg.part");

    let mut last_error = String::new();
    for attempt in 0..3 {
        if !running.load(Ordering::Relaxed) {
            return Outcome::Failed("interrupted".into());
        }
        if attempt > 0 {
            time::sleep(Duration::from_secs(5)).await;
        }
        let mut response = match requests::download_album_cover(&client, &url, &running).await {
            Ok(response) => response,
            Err(error) => return Outcome::Failed(error),
        };

        match save_stream(&mut response, &part_path, &cover_path).await {
            Ok(()) => return Outcome::Downloaded,
            Err(error) => {
                tracing::warn!("cover transfer failed: {error}");
                last_error = format!("cover transfer failed: {error}");
            }
        }
    }

    tracing::error!("failed cover for {title} after 3 attempts");
    Outcome::Failed(last_error)
}

/// Promote only a completely received body, after all buffered bytes reach the file.
async fn save_stream(
    response: &mut Response,
    part_path: &Path,
    final_path: &Path,
) -> std::io::Result<()> {
    let part = PartFile::new(part_path.to_owned());
    write_stream(response, &part.path).await?;
    fs::rename(&part.path, final_path).await?;
    part.promoted();
    Ok(())
}

/// Fails if the body ends early, which reqwest detects from Content-Length or chunked
/// framing.
async fn write_stream(response: &mut Response, part_path: &Path) -> std::io::Result<()> {
    let mut file = BufWriter::new(File::create(part_path).await?);
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| std::io::Error::other(error.without_url()))?
    {
        file.write_all(&chunk).await?;
    }
    file.flush().await
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};

    use super::*;

    /// Serves one raw HTTP response and returns the client's view of it.
    async fn serve(raw: Vec<u8>) -> Response {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let _ = socket.read(&mut [0; 4096]);
            socket.write_all(&raw).unwrap();
        });
        Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!("http://{addr}/"))
            .send()
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn flushes_tail_and_does_not_promote_interrupted_streams() {
        let dir = std::env::temp_dir().join(format!("lucida-stream-test-{}", std::process::id()));
        fs::create_dir_all(&dir).await.unwrap();
        let part = dir.join("audio.part");
        let final_path = dir.join("audio.flac");
        let payload = vec![123; 8193];
        let mut complete = b"HTTP/1.1 200 OK\r\nContent-Length: 8193\r\n\r\n".to_vec();
        complete.extend_from_slice(&payload);
        save_stream(&mut serve(complete).await, &part, &final_path)
            .await
            .unwrap();
        assert_eq!(fs::read(&final_path).await.unwrap(), payload);
        for truncated in [
            &b"HTTP/1.1 200 OK\r\nContent-Length: 8193\r\n\r\n0000000"[..],
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n7\r\n0000000\r\n",
        ] {
            let mut response = serve(truncated.to_vec()).await;
            assert!(
                save_stream(&mut response, &part, &final_path)
                    .await
                    .is_err()
            );
            assert_eq!(fs::read(&final_path).await.unwrap(), payload);
            assert!(
                !part.exists(),
                "failed transfers must not leave .part files"
            );
        }
        fs::remove_dir_all(dir).await.unwrap();
    }
}
