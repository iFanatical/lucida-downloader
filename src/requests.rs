use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use reqwest::header::CONTENT_TYPE;
use reqwest::{Client, RequestBuilder, Response, StatusCode, Url};
use tokio::time;

use crate::models::{
    Account, Availability, DownloadConfig, Token, Track, TrackDownload, TrackDownloadRequest,
    TrackDownloadResult, TrackDownloadStatus, Upload,
};

#[derive(Debug)]
pub enum JobError {
    Unavailable,
    Failed(String),
}

/// What to do with a successful response.
enum Next<T> {
    Done(T),
    Retry(String),
}

const ATTEMPTS: u32 = 3;

/// A processing job that answers with these is gone; the caller must start a new one.
const DEAD_JOB_STATUS_CODES: [StatusCode; 1] = [StatusCode::INTERNAL_SERVER_ERROR];

/// Client errors that a retry of the same request cannot fix.
const fn is_terminal(status: StatusCode) -> bool {
    matches!(status.as_u16(), 400 | 401 | 403 | 404 | 410 | 422)
}

/// Sends a request up to three times with 2/4-second backoff, retrying transport errors,
/// non-terminal statuses, and responses that `on_ok` asks to retry.
async fn with_retries<T, F: Future<Output = Next<T>>>(
    what: &str,
    running: &AtomicBool,
    stop_on: &[StatusCode],
    request: impl Fn() -> RequestBuilder,
    mut on_ok: impl FnMut(Response) -> F,
) -> Result<T, String> {
    let mut last_error = format!("{what} got no response");
    for attempt in 0..ATTEMPTS {
        if attempt > 0 {
            if !running.load(Ordering::Relaxed) {
                return Err("interrupted".into());
            }
            time::sleep(Duration::from_secs(2u64.pow(attempt))).await;
        }
        let response = match request().send().await {
            Ok(response) => response,
            Err(error) => {
                last_error = format!("{what} failed: {}", error.without_url());
                tracing::warn!("{last_error}");
                continue;
            }
        };

        let status = response.status();
        if status == StatusCode::OK {
            match on_ok(response).await {
                Next::Done(value) => return Ok(value),
                Next::Retry(error) => {
                    tracing::warn!("{error}");
                    last_error = error;
                }
            }
            continue;
        }

        last_error = format!("{what} returned HTTP {status}");
        if is_terminal(status) || stop_on.contains(&status) {
            tracing::info!("{last_error}; not retrying");
            return Err(last_error);
        }
        tracing::warn!("{last_error}");
    }
    Err(last_error)
}

pub async fn check_availability(client: &Client) -> Availability {
    let Ok(response) = client.get("https://lucida.to/").send().await else {
        return Availability::Unavailable;
    };

    match response.status() {
        StatusCode::OK => Availability::Available,
        StatusCode::FORBIDDEN => Availability::Captcha,
        _ => Availability::Unavailable,
    }
}

pub async fn resolve_album(
    client: &Client,
    url: &str,
    country: &str,
    running: &AtomicBool,
) -> Result<String, String> {
    let request_url =
        Url::parse_with_params("https://lucida.to/", &[("url", url), ("country", country)])
            .unwrap();
    with_retries(
        "album request",
        running,
        &[],
        || {
            client
                .get(request_url.clone())
                .timeout(Duration::from_secs(45))
        },
        |response| async move {
            match response.text().await {
                Ok(html) => Next::Done(html),
                Err(error) => {
                    Next::Retry(format!("album response failed: {}", error.without_url()))
                }
            }
        },
    )
    .await
}

pub async fn request_track_download(
    client: &Client,
    track: &Track,
    token_expiry: u64,
    config: &DownloadConfig,
    running: &AtomicBool,
) -> Result<TrackDownload, JobError> {
    with_retries(
        "job request",
        running,
        &[],
        || {
            client
                .post("https://lucida.to/api/load?url=%2Fapi%2Ffetch%2Fstream%2Fv2")
                .json(&TrackDownloadRequest {
                    account: Account {
                        id: &config.country,
                        r#type: "country",
                    },
                    compat: false,
                    downscale: "original",
                    handoff: true,
                    metadata: config.metadata,
                    private: config.private,
                    token: Token {
                        expiry: token_expiry,
                        primary: track.csrf.as_deref(),
                        secondary: track.csrf_fallback.as_deref(),
                    },
                    upload: Upload { enabled: false },
                    url: &track.url,
                })
                .timeout(Duration::from_secs(45))
        },
        |response| async move {
            match response.json().await {
                Ok(TrackDownloadResult::Ok(track_download)) => Next::Done(Ok(track_download)),
                Ok(TrackDownloadResult::Error { error }) if unavailable(&error) => {
                    tracing::warn!("skipping unavailable track {}: {error}", track.title);
                    Next::Done(Err(JobError::Unavailable))
                }
                Ok(TrackDownloadResult::Error { error }) => {
                    Next::Retry(format!("lucida error: {error}"))
                }
                Err(_) => Next::Retry("invalid job response".into()),
            }
        },
    )
    .await
    .unwrap_or_else(|error| Err(JobError::Failed(error)))
}

pub async fn track_download_status(
    client: &Client,
    stream: &TrackDownload,
    running: &AtomicBool,
) -> Option<TrackDownloadStatus> {
    let url = format!(
        "https://{}.lucida.to/api/fetch/request/{}",
        stream.server, stream.handoff
    );
    with_retries(
        "status request",
        running,
        &DEAD_JOB_STATUS_CODES,
        || client.get(&url).timeout(Duration::from_secs(45)),
        |response| async move {
            match response.json().await {
                Ok(status) => Next::Done(status),
                Err(error) => Next::Retry(format!(
                    "invalid processing response: {}",
                    error.without_url()
                )),
            }
        },
    )
    .await
    .ok()
}

/// Returns the audio response, ready to stream, and its MIME type.
pub async fn download_track(
    client: &Client,
    stream: &TrackDownload,
    running: &AtomicBool,
) -> Option<(Response, String)> {
    download_track_from_url(
        client,
        &format!(
            "https://{}.lucida.to/api/fetch/request/{}/download",
            stream.server, stream.handoff
        ),
        running,
    )
    .await
}

async fn download_track_from_url(
    client: &Client,
    url: &str,
    running: &AtomicBool,
) -> Option<(Response, String)> {
    with_retries(
        "audio request",
        running,
        &DEAD_JOB_STATUS_CODES,
        || client.get(url),
        |response| async move {
            let mime_type = response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            if mime_type.is_none() {
                tracing::warn!("missing or invalid Content-Type");
            }
            Next::Done(mime_type.map(|mime_type| (response, mime_type)))
        },
    )
    .await
    .ok()
    .flatten()
}

/// Returns the cover response, ready to stream.
pub async fn download_album_cover(
    client: &Client,
    url: &str,
    running: &AtomicBool,
) -> Result<Response, String> {
    with_retries(
        "cover request",
        running,
        &[],
        || client.get(url),
        |response| async move { Next::Done(response) },
    )
    .await
}

/// Only explicit streaming-availability messages are terminal; unknown errors retry.
pub fn unavailable(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    [
        "not available for streaming",
        "unavailable for streaming",
        "not streamable",
    ]
    .iter()
    .any(|pattern| message.contains(pattern))
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_explicit_streaming_unavailability_is_terminal() {
        assert!(super::unavailable(
            "This track is NOT AVAILABLE FOR STREAMING"
        ));
        assert!(super::unavailable("Track is not streamable"));
        assert!(!super::unavailable("Server unavailable"));
        assert!(!super::unavailable("Cannot contact any valid server"));
    }
}

#[cfg(test)]
mod http_tests {
    use super::*;
    use std::io::{Read, Write};

    #[tokio::test]
    async fn missing_job_returns_to_caller_without_retrying_dead_url() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = [0; 4096];
            assert!(socket.read(&mut request).unwrap() > 0);
            socket
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
        });
        let client = Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let result = time::timeout(
            Duration::from_secs(1),
            download_track_from_url(
                &client,
                &format!("http://{addr}/expired/download"),
                &AtomicBool::new(true),
            ),
        )
        .await
        .expect("404 must return immediately to create a fresh job");
        assert!(result.is_none());
        server.join().unwrap();
    }
}
