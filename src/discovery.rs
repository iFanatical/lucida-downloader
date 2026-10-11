//! Public artist-page discovery. Amazon's rendered catalog may be incomplete.
use std::collections::HashSet;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

use reqwest::{Client, Url};
use scraper::{Html, Selector};

// Amazon serves catalog HTML to this user agent; the normal browser response is a JS shell.
const CATALOG_AGENT: &str =
    "Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)";

#[derive(Debug)]
struct Release {
    url: String,
    title: String,
    description: String,
}

fn artist_url(input: &str) -> Result<Url, String> {
    let mut url = Url::parse(input).map_err(|_| "Expected a full Amazon Music artist URL")?;
    if url.scheme() != "https"
        || !matches!(
            url.host_str(),
            Some(
                "music.amazon.com"
                    | "music.amazon.co.uk"
                    | "music.amazon.de"
                    | "music.amazon.fr"
                    | "music.amazon.it"
                    | "music.amazon.es"
                    | "music.amazon.co.jp"
                    | "music.amazon.ca"
                    | "music.amazon.com.au"
                    | "music.amazon.com.br"
                    | "music.amazon.in"
                    | "music.amazon.com.mx"
            )
        )
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return Err("Use an https://music.amazon.../artists/ARTIST_ID URL".into());
    }
    let id = artist_id(&url)
        .ok_or("URL must identify an Amazon Music artist")?
        .to_owned();
    url.set_path(&format!("/artists/{id}"));
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

fn artist_id(url: &Url) -> Option<&str> {
    if let Some(id) = url
        .path()
        .strip_prefix("/browse/music-items/artist/")
        .and_then(|path| path.strip_suffix("/popular-albums"))
    {
        return valid_id(id).then_some(id);
    }
    let mut parts = url.path_segments()?;
    // Amazon also publishes locale-prefixed URLs such as /es-us/artists/ASIN/name.
    let first = parts.next()?;
    if first != "artists" && parts.next()? != "artists" {
        return None;
    }
    let id = parts.next()?;
    valid_id(id).then_some(id)
}

fn valid_id(id: &str) -> bool {
    id.len() == 10
        && id
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
}

fn parse_releases(html: &str, artist: &Url, all_releases: bool) -> Result<Vec<Release>, String> {
    parse_page(html, artist, all_releases, false)
}

fn parse_page(
    html: &str,
    artist: &Url,
    all_releases: bool,
    popular_albums: bool,
) -> Result<Vec<Release>, String> {
    let document = Html::parse_document(html);
    let cards = Selector::parse("music-vertical-item[primary-href][secondary-href]").unwrap();
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    let mut recognized = 0;
    for card in document.select(&cards) {
        let attrs = card.value();
        let Some(owner) = attrs
            .attr("secondary-href")
            .and_then(|value| artist.join(value).ok())
        else {
            continue;
        };
        if owner.origin() != artist.origin() || artist_id(&owner) != artist_id(artist) {
            continue;
        }
        let description = attrs.attr("secondary-text").unwrap_or_default();
        let kind = description.split('•').next().unwrap_or_default().trim();
        let typed = ["Album", "EP", "Single"]
            .iter()
            .any(|value| kind.eq_ignore_ascii_case(value));
        // Popular Albums cards show the artist name, not a release-type label.
        // Only that explicitly scoped page may supply the album type implicitly.
        if !typed && !popular_albums {
            continue;
        }
        recognized += 1;
        if !all_releases && typed && !kind.eq_ignore_ascii_case("Album") {
            continue;
        }
        let Some(mut url) = attrs
            .attr("primary-href")
            .and_then(|value| artist.join(value).ok())
        else {
            continue;
        };
        if url.origin() != artist.origin() {
            continue;
        }
        // A track link points into an album but must not become a release entry.
        if url
            .query_pairs()
            .any(|(key, _)| key.eq_ignore_ascii_case("trackAsin"))
        {
            continue;
        }
        let Some(id) = url
            .path()
            .strip_prefix("/albums/")
            .and_then(|path| path.split('/').next())
            .filter(|id| valid_id(id))
            .map(str::to_owned)
        else {
            continue;
        };
        if !seen.insert(id.clone()) {
            continue;
        }
        url.set_path(&format!("/albums/{id}"));
        url.set_query(None);
        url.set_fragment(None);
        result.push(Release {
            url: url.into(),
            title: attrs.attr("primary-text").unwrap_or("Untitled").to_owned(),
            description: description.to_owned(),
        });
    }
    if recognized == 0 {
        return Err("No recognizable artist release cards found. Amazon may have returned a challenge, a JavaScript-only page, or changed its layout.".into());
    }
    if result.is_empty() {
        return Err("No albums found in this page. Try --all-releases to include singles and EPs, or --artist-html with an expanded page.".into());
    }
    Ok(result)
}

async fn fetch_artist(url: &Url) -> Result<String, String> {
    // Separate client: never send the downloader's Lucida clearance cookie to Amazon.
    let origin = url.origin();
    let client = Client::builder()
        .user_agent(CATALOG_AGENT)
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= 5 {
                attempt.error("Too many artist-page redirects")
            } else if attempt.url().origin() == origin {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
        .build()
        .map_err(|error| error.to_string())?;
    for attempt in 0..3 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(2u64.pow(attempt))).await;
        }
        let response = client
            .get(url.clone())
            .header("Accept-Language", "en-US,en;q=0.9")
            .send()
            .await;
        let mut response = match response {
            Ok(response) => response,
            Err(error) if attempt == 2 => return Err(error.without_url().to_string()),
            Err(_) => continue,
        };
        let status = response.status();
        if status.is_server_error() || status.as_u16() == 429 {
            continue;
        }
        if !status.is_success() {
            return Err(format!(
                "Amazon returned HTTP {status}; open the artist page in your browser or use --artist-html."
            ));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| error.without_url().to_string())?
        {
            if bytes.len() + chunk.len() > 16 * 1024 * 1024 {
                return Err("Artist page exceeded 16 MiB".into());
            }
            bytes.extend_from_slice(&chunk);
        }
        return String::from_utf8(bytes).map_err(|_| "Artist page was not UTF-8".into());
    }
    Err("Amazon remained unavailable or rate-limited after three attempts".into())
}

fn write_list(path: &Path, contents: &str) -> Result<(), String> {
    // Refuse to overwrite an existing hand-edited album list.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| format!("Cannot create {}: {error}", path.display()))?;
    file.write_all(contents.as_bytes())
        .map_err(|error| format!("Cannot write {}: {error}", path.display()))
}

fn combine_pages(
    artist: Result<Vec<Release>, String>,
    popular: Result<Vec<Release>, String>,
) -> Result<Vec<Release>, String> {
    let mut combined = Vec::new();
    let mut seen = HashSet::new();
    for (label, page) in [("Artist page", artist), ("Popular Albums page", popular)] {
        match page {
            Ok(releases) => {
                let count = releases.len();
                let before = combined.len();
                for release in releases {
                    // Both parsers canonicalize URLs to /albums/ASIN on the same origin.
                    if seen.insert(release.url.clone()) {
                        combined.push(release);
                    }
                }
                eprintln!(
                    "{label}: {count} unique releases, {} added",
                    combined.len() - before
                );
            }
            Err(error) => {
                eprintln!("Warning: {label} could not be used: {error}. Coverage may be reduced.");
            }
        }
    }
    if combined.is_empty() {
        return Err("Neither page supplied usable releases; no list was written".into());
    }
    Ok(combined)
}

pub async fn run(
    input: &str,
    saved_html: Option<&Path>,
    save_list: Option<&Path>,
    all_releases: bool,
) -> Result<(), String> {
    let artist = artist_url(input)?;
    let releases = if let Some(path) = saved_html {
        let html = tokio::fs::read_to_string(path)
            .await
            .map_err(|error| format!("Cannot read {}: {error}", path.display()))?;
        let popular = Url::parse(input).is_ok_and(|url| url.path().ends_with("/popular-albums"));
        let releases = if popular {
            parse_page(&html, &artist, all_releases, true)?
        } else {
            parse_releases(&html, &artist, all_releases)?
        };
        eprintln!("Saved page: {} unique releases", releases.len());
        releases
    } else {
        let mut popular = artist.clone();
        popular.set_path(&format!(
            "/browse/music-items/artist/{}/popular-albums",
            artist_id(&artist).unwrap()
        ));
        let (artist_html, popular_html) =
            tokio::join!(fetch_artist(&artist), fetch_artist(&popular));
        let artist_releases =
            artist_html.and_then(|html| parse_releases(&html, &artist, all_releases));
        let popular_releases =
            popular_html.and_then(|html| parse_page(&html, &artist, all_releases, true));
        combine_pages(artist_releases, popular_releases)?
    };
    for release in &releases {
        eprintln!(
            "{} — {}",
            release.title.replace(['\n', '\r'], " "),
            release.description
        );
    }
    eprintln!(
        "Found {} unique release URLs. This is the catalog visible in the scanned pages, not a verified complete discography. Amazon may truncate large catalogs; use an expanded saved page with --artist-html for additional releases.",
        releases.len()
    );
    let contents = releases
        .iter()
        .map(|release| release.url.as_str())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    match save_list {
        Some(path) => {
            write_list(path, &contents)?;
            eprintln!(
                "Saved {}. Review it, then download with lucida -f {}",
                path.display(),
                path.display()
            );
        }
        None => std::io::stdout()
            .lock()
            .write_all(contents.as_bytes())
            .map_err(|error| error.to_string())?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    const ARTIST: &str = "https://music.amazon.com/artists/B000SZHO06/mazzy-star";
    fn card(id: &str, kind: &str, owner: &str) -> String {
        format!(
            r#"<music-vertical-item primary-href="/albums/{id}" primary-text="Title &amp; More" secondary-text="{kind} • 2020" secondary-href="/artists/{owner}"></music-vertical-item>"#
        )
    }
    #[test]
    fn filters_tracks_other_artists_and_singles_and_deduplicates() {
        let html = card("B000SZH66S", "Album", "B000SZHO06")
            + &card("B000SZH66S?ref=x", "Album", "B000SZHO06")
            + &card("B000TENKV8", "Single", "B000SZHO06")
            + &card("B000TEPFDO", "EP", "B000SZHO06")
            + &card("B00ENV1HPY", "Album", "B00157GJ20")
            + &card("B00ENV1HPY?trackAsin=B000SZH66S", "Album", "B000SZHO06");
        let artist = artist_url(ARTIST).unwrap();
        let albums = parse_releases(&html, &artist, false).unwrap();
        assert_eq!(albums.len(), 1);
        assert_eq!(albums[0].url, "https://music.amazon.com/albums/B000SZH66S");
        assert_eq!(albums[0].title, "Title & More");
        assert_eq!(parse_releases(&html, &artist, true).unwrap().len(), 3);
    }
    #[test]
    fn rejects_non_artist_inputs_and_unusable_pages() {
        for url in [
            "https://evil.example/artists/B000SZHO06",
            "https://music.amazon.com.evil.example/artists/B000SZHO06",
            "https://music.amazon.com/albums/B000SZH66S",
            "http://music.amazon.com/artists/B000SZHO06",
        ] {
            assert!(artist_url(url).is_err());
        }
        let artist = artist_url(ARTIST).unwrap();
        assert!(parse_releases("<html>challenge</html>", &artist, false).is_err());
        assert_eq!(
            artist_url("https://music.amazon.com/es-us/artists/B000SZHO06/name?ref=x").unwrap(),
            artist
        );
    }
    #[test]
    fn real_artist_page_contains_four_albums_and_eight_releases() {
        let html = include_str!("../tests/fixtures/amazon-artist.html");
        let artist = artist_url(ARTIST).unwrap();
        let albums = parse_releases(html, &artist, false).unwrap();
        assert_eq!(albums.len(), 4);
        assert!(
            albums
                .iter()
                .any(|album| album.title == "She Hangs Brightly")
        );
        assert_eq!(parse_releases(html, &artist, true).unwrap().len(), 8);
    }
    #[test]
    fn discovery_cli_is_separate_from_downloads() {
        use crate::models::Cli;
        use clap::Parser;
        assert!(
            Cli::try_parse_from(["lucida", "--discover-artist", ARTIST, "--all-releases"]).is_ok()
        );
        assert!(
            Cli::try_parse_from(["lucida", "--discover-artist", ARTIST, "-f", "albums.txt"])
                .is_err()
        );
        assert!(Cli::try_parse_from(["lucida", "--save-list", "albums.txt"]).is_err());
        assert!(Cli::try_parse_from(["lucida", "-f", "albums.txt"]).is_ok());
    }
    #[test]
    fn rem_pages_combine_to_27_albums_preserving_editions() {
        let artist = artist_url("https://music.amazon.com/artists/B000QJJS9O").unwrap();
        let first = parse_releases(
            include_str!("../tests/fixtures/rem-artist.html"),
            &artist,
            false,
        )
        .unwrap();
        let second = parse_page(
            include_str!("../tests/fixtures/rem-popular-albums.html"),
            &artist,
            false,
            true,
        )
        .unwrap();
        assert_eq!(first.len(), 19);
        assert_eq!(second.len(), 20);
        let combined = combine_pages(Ok(first), Ok(second)).unwrap();
        assert_eq!(combined.len(), 27);
        for title in ["Reveal", "Up", "Collapse Into Now", "Eponymous"] {
            assert!(
                combined.iter().any(|release| release.title == title),
                "Missing {title}"
            );
        }
        for id in ["B01N3Y8JVC", "B01LF43LB4"] {
            assert!(
                combined.iter().any(|release| release.url.ends_with(id)),
                "Missing edition {id}"
            );
        }
    }

    #[test]
    fn popular_album_context_does_not_leak_into_artist_parsing() {
        let artist = artist_url(ARTIST).unwrap();
        let html = card("B000SZH66S", "Mazzy Star", "B000SZHO06")
            + &card("B000TENKV8", "Single", "B000SZHO06")
            + &card("B000TEPFDO", "EP", "B000SZHO06")
            + &card("B00ENV1HPY", "Other Artist", "B00157GJ20");
        assert!(parse_releases(&html, &artist, false).is_err());
        let albums = parse_page(&html, &artist, false, true).unwrap();
        assert_eq!(albums.len(), 1);
        assert_eq!(albums[0].url, "https://music.amazon.com/albums/B000SZH66S");
        assert_eq!(
            artist_url(
                "https://music.amazon.com/browse/music-items/artist/B000SZHO06/popular-albums"
            )
            .unwrap(),
            artist
        );
        assert!(
            artist_url(
                "https://music.amazon.com/browse/music-items/artist/B000SZHO06/popular-tracks"
            )
            .is_err()
        );
    }

    #[test]
    fn one_failed_page_keeps_the_other_page_results() {
        let artist = artist_url(ARTIST).unwrap();
        let page = || parse_releases(&card("B000SZH66S", "Album", "B000SZHO06"), &artist, false);
        assert_eq!(
            combine_pages(page(), Err("HTTP 503".into())).unwrap().len(),
            1
        );
        assert_eq!(
            combine_pages(Err("Invalid HTML".into()), page())
                .unwrap()
                .len(),
            1
        );
        assert!(combine_pages(Err("HTTP 403".into()), Err("HTTP 503".into())).is_err());
    }

    #[test]
    fn preserves_existing_review_lists() {
        let path = std::env::temp_dir().join(format!("lucida-list-test-{}", std::process::id()));
        std::fs::write(&path, "existing list").unwrap();
        assert!(write_list(&path, "new list").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "existing list");
        std::fs::remove_file(path).unwrap();
    }
}
