// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Off-thread `yt-dlp` driver for explicit YouTube audio replacements.

use std::{
    fs,
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Sender},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use sustain_domain::TrackId;
use sustain_metadata::audio_format_from_path;
use tempfile::TempDir;
use url::Url;

#[derive(Debug)]
pub(crate) struct StagedYoutubeAudio {
    pub(crate) _directory: TempDir,
    pub(crate) path: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum YoutubeAudioDownloadError {
    InvalidUrl,
    YtDlpUnavailable,
    DownloadFailed,
    OutputRejected,
    Cancelled,
}

#[derive(Debug)]
pub struct YoutubeAudioDownloadResult {
    pub(crate) track_id: TrackId,
    pub(crate) outcome: Result<StagedYoutubeAudio, YoutubeAudioDownloadError>,
}

struct YoutubeAudioDownloadRequest {
    track_id: TrackId,
    url: String,
}

pub(crate) struct YoutubeAudioDownloader {
    sender: Option<Sender<YoutubeAudioDownloadRequest>>,
    shutdown_requested: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl YoutubeAudioDownloader {
    pub(crate) fn start(
        result_sink: async_channel::Sender<YoutubeAudioDownloadResult>,
    ) -> std::io::Result<Self> {
        let (sender, receiver) = mpsc::channel::<YoutubeAudioDownloadRequest>();
        let shutdown_requested = Arc::new(AtomicBool::new(false));
        let worker_shutdown_requested = shutdown_requested.clone();
        let handle = thread::Builder::new()
            .name("sustain-youtube-audio-downloader".to_owned())
            .spawn(move || {
                while let Ok(request) = receiver.recv() {
                    let result = YoutubeAudioDownloadResult {
                        track_id: request.track_id,
                        outcome: download(&request.url, &worker_shutdown_requested),
                    };
                    let _ = result_sink.send_blocking(result);
                    if worker_shutdown_requested.load(Ordering::Relaxed) {
                        break;
                    }
                }
            })?;
        Ok(Self {
            sender: Some(sender),
            shutdown_requested,
            handle: Some(handle),
        })
    }

    pub(crate) fn submit(&self, track_id: TrackId, url: String) -> bool {
        self.sender.as_ref().is_some_and(|sender| {
            sender
                .send(YoutubeAudioDownloadRequest { track_id, url })
                .is_ok()
        })
    }

    pub(crate) fn shutdown(mut self) {
        self.shutdown_requested.store(true, Ordering::Relaxed);
        self.sender.take();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for YoutubeAudioDownloader {
    fn drop(&mut self) {
        self.shutdown_requested.store(true, Ordering::Relaxed);
        self.sender.take();
    }
}

fn download(
    url: &str,
    shutdown_requested: &AtomicBool,
) -> Result<StagedYoutubeAudio, YoutubeAudioDownloadError> {
    validate_youtube_url(url)?;
    if shutdown_requested.load(Ordering::Relaxed) {
        return Err(YoutubeAudioDownloadError::Cancelled);
    }
    let directory = tempfile::tempdir().map_err(|_| YoutubeAudioDownloadError::DownloadFailed)?;
    let result_path = directory.path().join("result-path.txt");
    let output_template = directory.path().join("audio.%(ext)s");
    let mut child = Command::new("yt-dlp")
        .args([
            "--ignore-config",
            "--no-playlist",
            "--extract-audio",
            "--audio-format",
            "best",
            "--print-to-file",
            "after_move:filepath",
        ])
        .arg(&result_path)
        .arg("--output")
        .arg(&output_template)
        .arg("--")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                YoutubeAudioDownloadError::YtDlpUnavailable
            } else {
                YoutubeAudioDownloadError::DownloadFailed
            }
        })?;
    let status = loop {
        if shutdown_requested.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(YoutubeAudioDownloadError::Cancelled);
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => thread::sleep(Duration::from_millis(100)),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(YoutubeAudioDownloadError::DownloadFailed);
            }
        }
    };
    if !status.success() {
        return Err(YoutubeAudioDownloadError::DownloadFailed);
    }

    let raw_path =
        fs::read_to_string(result_path).map_err(|_| YoutubeAudioDownloadError::OutputRejected)?;
    let path = PathBuf::from(raw_path.trim());
    let path = fs::canonicalize(path).map_err(|_| YoutubeAudioDownloadError::OutputRejected)?;
    let canonical_directory = fs::canonicalize(directory.path())
        .map_err(|_| YoutubeAudioDownloadError::OutputRejected)?;
    if !path.starts_with(canonical_directory)
        || !fs::metadata(&path)
            .map(|metadata| metadata.is_file())
            .unwrap_or(false)
        || audio_format_from_path(&path).is_err()
    {
        return Err(YoutubeAudioDownloadError::OutputRejected);
    }

    Ok(StagedYoutubeAudio {
        _directory: directory,
        path,
    })
}

fn validate_youtube_url(raw: &str) -> Result<(), YoutubeAudioDownloadError> {
    let url = Url::parse(raw.trim()).map_err(|_| YoutubeAudioDownloadError::InvalidUrl)?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(YoutubeAudioDownloadError::InvalidUrl);
    }
    let host = url
        .host_str()
        .map(str::to_ascii_lowercase)
        .ok_or(YoutubeAudioDownloadError::InvalidUrl)?;
    if host == "youtu.be" || host == "youtube.com" || host.ends_with(".youtube.com") {
        Ok(())
    } else {
        Err(YoutubeAudioDownloadError::InvalidUrl)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_youtube_video_hosts() {
        for url in [
            "https://youtu.be/abc",
            "https://www.youtube.com/watch?v=abc",
            "https://music.youtube.com/watch?v=abc",
        ] {
            assert_eq!(validate_youtube_url(url), Ok(()));
        }
    }

    #[test]
    fn rejects_non_youtube_and_non_http_urls() {
        for url in [
            "https://example.com/watch?v=abc",
            "file:///tmp/audio.mp3",
            "not a url",
        ] {
            assert_eq!(
                validate_youtube_url(url),
                Err(YoutubeAudioDownloadError::InvalidUrl)
            );
        }
    }
}
