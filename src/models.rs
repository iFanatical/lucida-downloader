use std::path::PathBuf;

use clap::{Parser, ValueEnum};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

pub const BASE_URL: &str = "https://lucida.to/";

#[expect(clippy::struct_excessive_bools)]
#[derive(Parser)]
#[command(arg_required_else_help = true)]
pub struct Cli {
    /// Discover Amazon artist album URLs without downloading music
    #[arg(long, value_name = "ARTIST_URL", conflicts_with_all = ["urls", "file"])]
    pub discover_artist: Option<String>,

    /// Save discovered URLs to a new text file (default: stdout)
    #[arg(long, requires = "discover_artist", value_name = "FILE")]
    pub save_list: Option<PathBuf>,

    /// Include EPs and singles in artist discovery
    #[arg(long, requires = "discover_artist")]
    pub all_releases: bool,

    /// Read an expanded Amazon artist page saved as HTML instead of fetching
    #[arg(long, requires = "discover_artist", value_name = "HTML_FILE")]
    pub artist_html: Option<PathBuf>,

    /// URLs to download
    pub urls: Vec<String>,

    /// files to read URLs from
    #[arg(short, long)]
    pub file: Vec<PathBuf>,

    /// custom path to download to
    #[arg(short, long)]
    pub output: Option<PathBuf>,

    /// overwrite already downloaded files
    #[arg(long)]
    pub force: bool,

    /// place all artist's singles in a "Singles" directory. their covers will
    /// not be downloaded
    #[arg(long)]
    pub group_singles: bool,

    /// use "<album> (year)" or "(year) <album>" directory name
    #[arg(value_enum, long)]
    pub album_year: Option<AlbumYear>,

    /// use "<artist> - <album>" format instead of nested "<artist>/<album>"
    /// directories
    #[arg(long)]
    pub flatten_directories: bool,

    /// country to use accounts from
    #[arg(long, default_value_t = String::from("auto"))]
    pub country: String,

    /// disable metadata embedding by lucida
    #[arg(long)]
    pub no_metadata: bool,

    /// hide tracks from recent downloads on lucida
    #[arg(long)]
    pub private: bool,

    /// amount of albums to download simultaneously
    #[arg(long, default_value_t = 1)]
    pub album_workers: usize,

    /// amount of tracks to download simultaneously for each album
    #[arg(long, default_value_t = 4)]
    pub track_workers: usize,

    /// skip downloading tracks in the album
    #[arg(long)]
    pub skip_tracks: bool,

    /// skip downloading album cover
    #[arg(long)]
    pub skip_cover: bool,

    /// set the `cf_clearance` cookie and the User-Agent header if Cloudflare is
    /// blocking your requests
    #[arg(long)]
    pub cf_clearance: Option<String>,

    /// the User-Agent header to use
    #[arg(long)]
    pub user_agent: Option<String>,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum AlbumYear {
    Append,
    Prepend,
}

pub enum ResolveAlbumError {
    ArtistUrl { name: String },
}

pub enum Availability {
    Available,
    Captcha,
    Unavailable,
}

#[derive(Clone)]
pub struct DownloadConfig {
    pub country: String,
    pub metadata: bool,
    pub private: bool,
}

#[derive(Clone, Copy)]
pub struct SkipConfig {
    pub tracks: bool,
    pub cover: bool,
}

pub struct AlbumInfo {
    pub title: String,
    pub release_year: Option<u16>,
    pub cover_artwork_url: Option<String>,
    pub artist_name: String,
    pub tracks: Vec<(Option<u32>, Track)>,
    pub track_count: u32,
}

impl AlbumInfo {
    pub fn new(info: Info, token: Option<String>) -> Result<Self, ResolveAlbumError> {
        match info {
            Info::Album {
                title,
                cover_artwork,
                artists,
                track_count,
                release_date,
                tracks,
            } => Ok(Self {
                title,
                release_year: year(release_date),
                cover_artwork_url: last_cover(cover_artwork),
                artist_name: first_artist(artists),
                tracks: numbered(tracks),
                track_count,
            }),
            Info::Playlist {
                title,
                cover_artwork,
                track_count,
                tracks,
            } => Ok(Self {
                title,
                release_year: None,
                cover_artwork_url: last_cover(cover_artwork),
                artist_name: "Playlists".into(),
                tracks: numbered(tracks),
                track_count,
            }),
            Info::Track {
                url,
                title,
                cover_artwork,
                artists,
                album,
                release_date,
                producers,
            } => {
                let track_cover = cover_artwork.and_then(last_cover);
                let track_artist = artists.first().map(|artist| artist.name.clone());
                let (album_title, release_year, cover_artwork_url, artist_name, track_count) =
                    match album {
                        Some(album) => (
                            album.title,
                            year(album.release_date),
                            last_cover(album.cover_artwork).or(track_cover),
                            album
                                .artists
                                .into_iter()
                                .next()
                                .map(|artist| artist.name)
                                .or(track_artist),
                            album.track_count.unwrap_or(1),
                        ),
                        None => (
                            title.clone(),
                            release_date.and_then(year),
                            track_cover,
                            track_artist,
                            1,
                        ),
                    };
                Ok(Self {
                    title: album_title,
                    release_year,
                    cover_artwork_url,
                    artist_name: artist_name.unwrap_or_else(|| UNKNOWN_ARTIST.into()),
                    tracks: vec![(
                        None,
                        Track {
                            title,
                            url,
                            artists,
                            producers,
                            csrf: token,
                            csrf_fallback: None,
                        },
                    )],
                    track_count,
                })
            }
            Info::Artist { name } => Err(ResolveAlbumError::ArtistUrl { name }),
        }
    }
}

const UNKNOWN_ARTIST: &str = "Unknown";

fn year(date: OffsetDateTime) -> Option<u16> {
    date.year().try_into().ok()
}

fn last_cover(mut covers: Vec<CoverArtwork>) -> Option<String> {
    covers.pop().map(|cover| cover.url)
}

fn first_artist(artists: Vec<Artist>) -> String {
    artists
        .into_iter()
        .next()
        .map_or_else(|| UNKNOWN_ARTIST.into(), |artist| artist.name)
}

/// Numbers tracks from 1, reversed so workers pop them in album order.
fn numbered(tracks: Vec<Track>) -> Vec<(Option<u32>, Track)> {
    let mut tracks: Vec<_> = (1..)
        .zip(tracks)
        .map(|(number, track)| (Some(number), track))
        .collect();
    tracks.reverse();
    tracks
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PageData {
    pub info: Info,
    pub original_service: Service,
    pub token: Option<String>,
    pub token_expiry: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
#[serde(tag = "type")]
pub enum Info {
    #[serde(rename_all = "camelCase")]
    Album {
        title: String,
        cover_artwork: Vec<CoverArtwork>,
        artists: Vec<Artist>,
        track_count: u32,
        #[serde(with = "time::serde::rfc3339")]
        release_date: OffsetDateTime,
        tracks: Vec<Track>,
    },
    #[serde(rename_all = "camelCase")]
    Playlist {
        title: String,
        cover_artwork: Vec<CoverArtwork>,
        track_count: u32,
        tracks: Vec<Track>,
    },
    #[serde(rename_all = "camelCase")]
    Track {
        url: String,
        title: String,
        cover_artwork: Option<Vec<CoverArtwork>>,
        artists: Vec<Artist>,
        album: Option<Album>,
        #[serde(with = "time::serde::rfc3339::option")]
        release_date: Option<OffsetDateTime>,
        producers: Option<Vec<String>>,
    },
    #[serde(rename_all = "camelCase")]
    Artist { name: String },
}

#[derive(Deserialize)]
pub struct CoverArtwork {
    pub url: String,
}

#[derive(Deserialize, Clone)]
pub struct Artist {
    pub name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Track {
    pub title: String,
    pub url: String,
    pub artists: Vec<Artist>,
    pub producers: Option<Vec<String>>,
    pub csrf: Option<String>,
    pub csrf_fallback: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Album {
    pub url: Option<String>,
    pub title: String,
    pub cover_artwork: Vec<CoverArtwork>,
    pub artists: Vec<Artist>,
    pub track_count: Option<u32>,
    #[serde(with = "time::serde::rfc3339")]
    pub release_date: OffsetDateTime,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Service {
    Qobuz,
    Tidal,
    Soundcloud,
    Amazon,
}

#[expect(clippy::struct_excessive_bools)]
#[derive(Serialize)]
pub struct TrackDownloadRequest<'a> {
    pub account: Account<'a>,
    pub compat: bool,
    pub downscale: &'static str,
    pub handoff: bool,
    pub metadata: bool,
    pub private: bool,
    pub token: Token<'a>,
    pub upload: Upload,
    pub url: &'a str,
}

#[derive(Serialize)]
pub struct Account<'a> {
    pub id: &'a str,
    pub r#type: &'static str,
}

#[derive(Serialize)]
pub struct Token<'a> {
    pub expiry: u64,
    pub primary: Option<&'a str>,
    pub secondary: Option<&'a str>,
}

#[derive(Serialize)]
pub struct Upload {
    pub enabled: bool,
}

#[derive(Deserialize)]
#[serde(untagged)]
pub enum TrackDownloadResult {
    Ok(TrackDownload),
    Error { error: String },
}

#[derive(Deserialize)]
pub struct TrackDownload {
    pub handoff: String,
    pub server: String,
}

#[derive(Debug, Deserialize)]
pub struct TrackDownloadStatus {
    pub status: String,
    pub message: String,
}
