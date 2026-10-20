use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::{env, process};

use clap::Parser;
use futures::future;
use models::{BASE_URL, Cli, DownloadConfig, SkipConfig};
use reqwest::header::{COOKIE, HeaderMap};
use reqwest::{ClientBuilder, Url};
use tokio::fs::File;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::signal;
use tracing::Instrument;

use crate::models::Availability;

mod discovery;
mod downloaders;
mod models;
mod numbering;
mod report;
mod requests;
mod text_utils;
mod workers;

const CAPTCHA_PROMPT: &str = concat!(
    "lucida requires you to complete a captcha!\n\n",
    "1. Open a new tab in your browser\n",
    "2. Open DevTools using F12 or Ctrl+Shift+I\n",
    "3. Select the Network tab\n",
    "4. Go to https://lucida.to/\n",
    "5. Complete the captcha\n",
    "6. Select one of the requests to lucida.to\n",
    "7. In the Request Headers section locate the Cookie and User-Agent headers\n",
    "8. Run the command again with two more arguments:\n",
    "  - set --cf-clearance to the value of the cf_clearance cookie from the Cookie header\n",
    "  - set --user-agent argument to the value of the User-Agent header; make sure to quote it!"
);

#[expect(
    clippy::too_many_lines,
    reason = "CLI validation and batch orchestration"
)]
#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    // Logs go to stderr so stdout stays clean for discovered URL lists.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .fmt_fields(tracing_subscriber::fmt::format::debug_fn(
            |writer, _, value| write!(writer, "{value:?}"),
        ))
        .init();

    let cli = Cli::parse();

    if let Some(artist) = &cli.discover_artist {
        return match discovery::run(
            artist,
            cli.artist_html.as_deref(),
            cli.save_list.as_deref(),
            cli.all_releases,
        )
        .await
        {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                tracing::error!("Artist discovery failed: {error}");
                ExitCode::FAILURE
            }
        };
    }

    if cli.album_workers == 0 || cli.track_workers == 0 {
        tracing::error!("Worker counts must be greater than zero");
        return ExitCode::FAILURE;
    }
    for url in &cli.urls {
        let path = Path::new(url);
        if path.is_file() || path.is_dir() {
            tracing::error!("{url} is a local path; pass it with -f/--file to read its URLs");
            return ExitCode::FAILURE;
        }
        if !valid_download_url(url) {
            tracing::error!("Invalid URL: {url}");
            return ExitCode::FAILURE;
        }
    }
    let mut urls = cli.urls;

    let mut files = Vec::new();
    for path in cli.file {
        if path.is_dir() {
            match url_lists(&path).await {
                Ok(lists) if lists.is_empty() => {
                    tracing::error!("No .txt URL lists found in {}", path.display());
                    return ExitCode::FAILURE;
                }
                Ok(lists) => {
                    tracing::info!("reading {} URL lists from {}", lists.len(), path.display());
                    files.extend(lists);
                }
                Err(error) => {
                    tracing::error!("Cannot read directory {}: {error}", path.display());
                    return ExitCode::FAILURE;
                }
            }
        } else {
            files.push(path);
        }
    }

    for path in files {
        let file = match File::open(&path).await {
            Ok(file) => file,
            Err(error) => {
                tracing::error!("Cannot open {}: {error}", path.display());
                return ExitCode::FAILURE;
            }
        };
        let mut lines = BufReader::new(file).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) if !line.trim().is_empty() => {
                    let url = line.trim();
                    if !valid_download_url(url) {
                        tracing::error!("Invalid URL in {}: {url}", path.display());
                        return ExitCode::FAILURE;
                    }
                    urls.push(url.to_owned());
                }
                Ok(Some(_)) => (),
                Ok(None) => break,
                Err(error) => {
                    tracing::error!("Cannot read {}: {error}", path.display());
                    return ExitCode::FAILURE;
                }
            }
        }
    }

    urls.reverse();

    if urls.is_empty() {
        tracing::error!("no URLs to download");
        return ExitCode::FAILURE;
    }

    let urls_len = urls.len();

    let client = {
        let mut client = ClientBuilder::new()
            .connect_timeout(std::time::Duration::from_secs(15))
            .read_timeout(std::time::Duration::from_secs(30));

        if let Some(user_agent) = &cli.user_agent {
            client = client.user_agent(user_agent);
        }

        if let Some(cf_clearance) = &cli.cf_clearance {
            let Ok(cookie) = format!("cf_clearance={cf_clearance}").try_into() else {
                tracing::error!("Invalid cookie header");
                return ExitCode::FAILURE;
            };
            client = client.default_headers(HeaderMap::from_iter([(COOKIE, cookie)]));
        }

        match client.build() {
            Ok(client) => client,
            Err(error) => {
                tracing::error!("Cannot build HTTP client: {error}");
                return ExitCode::FAILURE;
            }
        }
    };

    match requests::check_availability(&client).await {
        Availability::Available => (),
        Availability::Captcha => {
            if cli.cf_clearance.is_some() && cli.user_agent.is_some() {
                tracing::error!(
                    "Your cf_clearance cookie and User-Agent header weren't accepted. They might be stale"
                );
            } else {
                tracing::error!("{CAPTCHA_PROMPT}");
            }

            return ExitCode::FAILURE;
        }
        Availability::Unavailable => {
            tracing::error!(
                "lucida seems to be unavailable right now. Visit the website: {BASE_URL}"
            );
            return ExitCode::FAILURE;
        }
    }

    if !cli.skip_tracks && !numbering::detect_ffprobe().await {
        tracing::warn!(
            "warning: ffprobe not found on PATH; standalone tracks without album information will keep unnumbered filenames (listed in the final report)"
        );
    }

    tracing::info!("downloading {urls_len} albums");

    let urls = Arc::new(Mutex::new(urls));
    let running = Arc::new(AtomicBool::new(true));
    let running_clone = running.clone();
    let worker_count = cli.album_workers.min(urls_len);

    tracing::info!("spawning {worker_count} album workers");

    tokio::spawn(async move {
        signal::ctrl_c().await.unwrap();
        running_clone.store(false, Ordering::Relaxed);
        tracing::warn!("Stopping gracefully");
        signal::ctrl_c().await.unwrap();
        process::exit(1);
    });

    let output = match cli.output.map_or_else(env::current_dir, Ok) {
        Ok(path) => path,
        Err(error) => {
            tracing::error!("Cannot locate output directory: {error}");
            return ExitCode::FAILURE;
        }
    };

    for result in future::join_all((1..=worker_count).map(|album_worker| {
        tokio::spawn(
            workers::run_album_worker(
                client.clone(),
                urls.clone(),
                output.clone(),
                cli.force,
                cli.group_singles,
                cli.album_year,
                cli.flatten_directories,
                DownloadConfig {
                    country: cli.country.clone(),
                    metadata: !cli.no_metadata,
                    private: cli.private,
                },
                cli.track_workers,
                SkipConfig {
                    tracks: cli.skip_tracks,
                    cover: cli.skip_cover,
                },
                running.clone(),
            )
            .instrument(tracing::info_span!("album", album_worker)),
        )
    }))
    .await
    {
        if let Err(error) = result {
            tracing::error!("album worker failed: {error}");
            return ExitCode::FAILURE;
        }
    }

    for url in urls
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
    {
        report::Item::new(url, report::Kind::Album).fail("not started (interrupted)");
    }
    tracing::info!("finished!");
    if report::finish(&output) && running.load(Ordering::Relaxed) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Lists the `.txt` files directly inside `directory`, by name, leaving out this tool's own
/// saved reports and retry lists.
async fn url_lists(directory: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut lists = Vec::new();
    let mut entries = tokio::fs::read_dir(directory).await?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        let is_list = path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("txt"));
        let is_saved_by_lucida =
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    report::SAVED_FILE_PREFIXES
                        .iter()
                        .any(|prefix| name.starts_with(prefix))
                });
        if is_list && !is_saved_by_lucida && tokio::fs::metadata(&path).await?.is_file() {
            lists.push(path);
        }
    }
    lists.sort();
    Ok(lists)
}

fn valid_download_url(value: &str) -> bool {
    Url::parse(value)
        .is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.host().is_some())
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn directory_lists_are_sorted_txt_files_without_saved_reports() {
        let directory =
            std::env::temp_dir().join(format!("lucida-lists-test-{}", std::process::id()));
        std::fs::create_dir_all(directory.join("nested.txt")).unwrap();
        for name in [
            "b.txt",
            "A.TXT",
            "notes.md",
            "lucida-report-1.txt",
            "lucida-failed-1.txt",
        ] {
            std::fs::write(directory.join(name), "").unwrap();
        }
        let lists = super::url_lists(&directory).await.unwrap();
        let names: Vec<_> = lists
            .iter()
            .map(|path| path.file_name().unwrap().to_str().unwrap())
            .collect();
        assert_eq!(names, ["A.TXT", "b.txt"]);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
