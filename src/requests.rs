use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use reqwest::header::CONTENT_TYPE;
use reqwest::{Client, StatusCode, Url};
use tokio::sync::mpsc::{self, Receiver};
use tokio::time;
use tracing::Instrument;

use crate::models::{
    Account, Availability, DownloadConfig, Token, Track, TrackDownload, TrackDownloadRequest,
    TrackDownloadResult, TrackDownloadStatus, Upload,
};

#[derive(Debug)]
pub enum JobError {
    Unavailable,
    Failed(String),
}

pub enum StreamEvent {
    Data(Vec<u8>),
    Finished,
    Failed,
}

const IRRECOVERABLE_STATUS_CODES: [StatusCode; 2] =
    [StatusCode::NOT_FOUND, StatusCode::INTERNAL_SERVER_ERROR];

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
    running: &Arc<AtomicBool>,
) -> Result<String, String> {
    let mut last_error = String::from("no response");
    for attempt in 0..3 {
        if attempt > 0 {
            time::sleep(Duration::from_secs(2u64.pow(attempt))).await;
        }
        let response = client
            .get(
                Url::parse_with_params("https://lucida.to/", &[("url", url), ("country", country)])
                    .unwrap(),
            )
            .timeout(Duration::from_secs(45))
            .send()
            .await;
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                last_error = format!("request failed: {}", error.without_url());
                tracing::warn!("{last_error}");
                continue;
            }
        };

        let status = response.status();
        if matches!(status.as_u16(), 400 | 401 | 403 | 404 | 410 | 422) {
            tracing::info!("request stopped: HTTP {status}");
            return Err(format!("lucida returned HTTP {status}"));
        }

        if status == StatusCode::OK {
            match response.text().await {
                Ok(html) => return Ok(html),
                Err(error) => {
                    last_error = format!("album response failed: {}", error.without_url());
                    tracing::warn!("{last_error}");
                    continue;
                }
            }
        }

        last_error = format!("lucida returned HTTP {status}");
        tracing::info!("received code {} when resolving album", status.as_u16());

        if !running.load(Ordering::Relaxed) {
            return Err("interrupted".into());
        }
    }
    Err(last_error)
}

pub async fn request_track_download(
    client: &Client,
    track: &Track,
    token_expiry: u64,
    config: &DownloadConfig,
    running: Arc<AtomicBool>,
) -> Result<TrackDownload, JobError> {
    let mut last_error = String::from("no response");
    for attempt in 0..3 {
        if attempt > 0 {
            time::sleep(Duration::from_secs(2u64.pow(attempt))).await;
        }
        let response = client
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
            .send()
            .await;
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                last_error = format!("job request failed: {}", error.without_url());
                tracing::warn!("{last_error}");
                continue;
            }
        };

        let status = response.status();
        if matches!(status.as_u16(), 400 | 401 | 403 | 404 | 410 | 422) {
            tracing::info!("request stopped: HTTP {status}");
            return Err(JobError::Failed(format!(
                "job request returned HTTP {status}"
            )));
        }

        if status == StatusCode::OK {
            if let Ok(track_download) = response.json().await {
                match track_download {
                    TrackDownloadResult::Ok(track_download) => return Ok(track_download),
                    TrackDownloadResult::Error { error, .. } => {
                        tracing::warn!("error when requesting track download: {error}");
                        last_error = format!("lucida error: {error}");
                        if unavailable(&error) {
                            tracing::warn!("skipping unavailable track {}", track.title);
                            return Err(JobError::Unavailable);
                        }

                        if !running.load(Ordering::Relaxed) {
                            return Err(JobError::Failed("interrupted".into()));
                        }
                    }
                }
            } else {
                tracing::warn!("invalid JSON when requesting track download");
                last_error = "invalid job response".into();

                if !running.load(Ordering::Relaxed) {
                    return Err(JobError::Failed("interrupted".into()));
                }
            }
        } else {
            tracing::warn!(
                "received code {} when requesting track download",
                status.as_u16()
            );
            last_error = format!("job request returned HTTP {status}");

            if !running.load(Ordering::Relaxed) {
                return Err(JobError::Failed("interrupted".into()));
            }
        }
    }
    Err(JobError::Failed(last_error))
}

pub async fn track_download_status(
    client: &Client,
    stream: &TrackDownload,
) -> Option<TrackDownloadStatus> {
    for attempt in 0..3 {
        if attempt > 0 {
            time::sleep(Duration::from_secs(2u64.pow(attempt))).await;
        }
        let response = client
            .get(format!(
                "https://{}.lucida.to/api/fetch/request/{}",
                stream.server, stream.handoff
            ))
            .timeout(Duration::from_secs(45))
            .send()
            .await;
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                tracing::warn!("request failed: {}", error.without_url());
                continue;
            }
        };

        let status = response.status();
        if matches!(status.as_u16(), 400 | 401 | 403 | 404 | 410 | 422) {
            tracing::info!("request stopped: HTTP {status}");
            return None;
        }

        if status == StatusCode::OK {
            match response.json().await {
                Ok(status) => return Some(status),
                Err(error) => {
                    tracing::warn!("invalid processing response: {}", error.without_url());
                    continue;
                }
            }
        }

        tracing::warn!(
            "received code {} when checking track processing status",
            status.as_u16()
        );

        if IRRECOVERABLE_STATUS_CODES.contains(&status) {
            return None;
        }
    }
    None
}

pub async fn download_track(
    client: &Client,
    stream: &TrackDownload,
) -> Option<(Receiver<StreamEvent>, String)> {
    download_track_from_url(
        client,
        &format!(
            "https://{}.lucida.to/api/fetch/request/{}/download",
            stream.server, stream.handoff
        ),
    )
    .await
}

async fn download_track_from_url(
    client: &Client,
    url: &str,
) -> Option<(Receiver<StreamEvent>, String)> {
    for attempt in 0..3 {
        if attempt > 0 {
            time::sleep(Duration::from_secs(2u64.pow(attempt))).await;
        }
        let response = client.get(url).send().await;
        let mut response = match response {
            Ok(response) => response,
            Err(error) => {
                tracing::warn!("request failed: {}", error.without_url());
                continue;
            }
        };

        let status = response.status();
        if matches!(status.as_u16(), 400 | 401 | 403 | 404 | 410 | 422) {
            tracing::info!("request stopped: HTTP {status}");
            return None;
        }

        if status == StatusCode::OK {
            let Some(mime_type) = response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
            else {
                tracing::warn!("missing or invalid Content-Type");
                return None;
            };

            let (tx, rx) = mpsc::channel(8);

            tokio::spawn(
                async move {
                    loop {
                        let result = response.chunk().await;

                        match result {
                            Ok(chunk) => {
                                if let Some(chunk) = chunk {
                                    if tx.send(StreamEvent::Data(chunk.to_vec())).await.is_err() {
                                        break;
                                    }
                                } else {
                                    let _ = tx.send(StreamEvent::Finished).await;
                                    break;
                                }
                            }
                            Err(err) => {
                                tracing::info!("error when downloading track audio: {err}");
                                let _ = tx.send(StreamEvent::Failed).await;
                                break;
                            }
                        }
                    }
                }
                .in_current_span(),
            );

            return Some((rx, mime_type));
        }

        tracing::info!(
            "received code {} when downloading track audio",
            status.as_u16()
        );

        if IRRECOVERABLE_STATUS_CODES.contains(&status) {
            return None;
        }
    }
    None
}

pub async fn download_album_cover(
    client: &Client,
    url: &str,
    running: Arc<AtomicBool>,
) -> Result<Receiver<StreamEvent>, String> {
    let mut last_error = String::from("no response");
    for attempt in 0..3 {
        if attempt > 0 {
            time::sleep(Duration::from_secs(2u64.pow(attempt))).await;
        }
        let response = client.get(url).send().await;
        let mut response = match response {
            Ok(response) => response,
            Err(error) => {
                last_error = format!("cover request failed: {}", error.without_url());
                tracing::warn!("{last_error}");
                continue;
            }
        };
        let status = response.status();
        if matches!(status.as_u16(), 400 | 401 | 403 | 404 | 410 | 422) {
            tracing::info!("request stopped: HTTP {status}");
            return Err(format!("cover request returned HTTP {status}"));
        }

        if status == StatusCode::OK {
            let (tx, rx) = mpsc::channel(8);

            tokio::spawn(
                async move {
                    loop {
                        let result = response.chunk().await;

                        match result {
                            Ok(chunk) => {
                                if let Some(chunk) = chunk {
                                    if tx.send(StreamEvent::Data(chunk.to_vec())).await.is_err() {
                                        break;
                                    }
                                } else {
                                    let _ = tx.send(StreamEvent::Finished).await;
                                    break;
                                }
                            }
                            Err(err) => {
                                tracing::info!("error when downloading album cover: {err}");

                                let _ = tx.send(StreamEvent::Failed).await;
                                break;
                            }
                        }
                    }
                }
                .in_current_span(),
            );

            return Ok(rx);
        }

        last_error = format!("cover request returned HTTP {status}");
        tracing::info!(
            "received code {} when downloading album cover from {url}",
            status.as_u16()
        );

        if !running.load(Ordering::Relaxed) {
            return Err("interrupted".into());
        }
    }
    Err(last_error)
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
            download_track_from_url(&client, &format!("http://{addr}/expired/download")),
        )
        .await
        .expect("404 must return immediately to create a fresh job");
        assert!(result.is_none());
        server.join().unwrap();
    }
}
