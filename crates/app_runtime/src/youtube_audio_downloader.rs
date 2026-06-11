// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Off-thread `yt-dlp` driver for explicit YouTube audio replacements.

use std::{
    fs,
    io::Read,
    os::unix::process::CommandExt,
    path::Path,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use rustix::process::{Pid, Signal, kill_process_group};
use sustain_domain::TrackId;
use sustain_metadata::audio_format_from_path;
use tempfile::TempDir;
use url::Url;

const MAX_YOUTUBE_DOWNLOAD_RUNTIME: Duration = Duration::from_secs(5 * 60);
const MAX_YOUTUBE_STAGED_BYTES: u64 = 64 * 1024 * 1024;
const MAX_YOUTUBE_OUTPUT_PATH_BYTES: u64 = 4096;
const MAX_YOUTUBE_STAGED_ENTRIES: usize = 1024;
const YOUTUBE_DOWNLOAD_POLL_INTERVAL: Duration = Duration::from_millis(100);
pub(crate) const MAX_YOUTUBE_REPLACEMENT_DURATION: Duration = Duration::from_secs(20 * 60);

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
    PayloadTooLarge,
    TimedOut,
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
    download_with_limits(
        url,
        shutdown_requested,
        Path::new("yt-dlp"),
        MAX_YOUTUBE_DOWNLOAD_RUNTIME,
        MAX_YOUTUBE_STAGED_BYTES,
    )
}

fn download_with_limits(
    url: &str,
    shutdown_requested: &AtomicBool,
    executable: &Path,
    max_runtime: Duration,
    max_staged_bytes: u64,
) -> Result<StagedYoutubeAudio, YoutubeAudioDownloadError> {
    validate_youtube_url(url)?;
    if shutdown_requested.load(Ordering::Relaxed) {
        return Err(YoutubeAudioDownloadError::Cancelled);
    }
    let directory = tempfile::tempdir().map_err(|_| YoutubeAudioDownloadError::DownloadFailed)?;
    let result_path = directory.path().join("result-path.txt");
    let output_template = directory.path().join("audio.%(ext)s");
    let max_filesize = max_staged_bytes.to_string();
    let duration_filter = format!("duration <= {}", MAX_YOUTUBE_REPLACEMENT_DURATION.as_secs());
    let mut child = Command::new(executable)
        .args([
            "--ignore-config",
            "--no-playlist",
            "--max-filesize",
            &max_filesize,
            "--match-filter",
            &duration_filter,
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
        .process_group(0)
        .spawn()
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                YoutubeAudioDownloadError::YtDlpUnavailable
            } else {
                YoutubeAudioDownloadError::DownloadFailed
            }
        })?;
    let started_at = Instant::now();
    let status = loop {
        if shutdown_requested.load(Ordering::Relaxed) {
            terminate_process_group(&mut child);
            return Err(YoutubeAudioDownloadError::Cancelled);
        }
        if started_at.elapsed() >= max_runtime {
            terminate_process_group(&mut child);
            return Err(YoutubeAudioDownloadError::TimedOut);
        }
        match staged_directory_size(directory.path(), max_staged_bytes) {
            Ok(size) if size > max_staged_bytes => {
                terminate_process_group(&mut child);
                return Err(YoutubeAudioDownloadError::PayloadTooLarge);
            }
            Ok(_) => {}
            Err(error) => {
                terminate_process_group(&mut child);
                return Err(error);
            }
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => thread::sleep(
                max_runtime
                    .saturating_sub(started_at.elapsed())
                    .min(YOUTUBE_DOWNLOAD_POLL_INTERVAL),
            ),
            Err(_) => {
                terminate_process_group(&mut child);
                return Err(YoutubeAudioDownloadError::DownloadFailed);
            }
        }
    };
    if !status.success() {
        return Err(YoutubeAudioDownloadError::DownloadFailed);
    }
    if staged_directory_size(directory.path(), max_staged_bytes)? > max_staged_bytes {
        return Err(YoutubeAudioDownloadError::PayloadTooLarge);
    }

    let mut raw_path = String::new();
    fs::File::open(result_path)
        .and_then(|file| {
            file.take(MAX_YOUTUBE_OUTPUT_PATH_BYTES + 1)
                .read_to_string(&mut raw_path)
        })
        .map_err(|_| YoutubeAudioDownloadError::OutputRejected)?;
    if raw_path.len() as u64 > MAX_YOUTUBE_OUTPUT_PATH_BYTES {
        return Err(YoutubeAudioDownloadError::OutputRejected);
    }
    let path = PathBuf::from(raw_path.trim());
    if !fs::symlink_metadata(&path)
        .map(|metadata| metadata.file_type().is_file())
        .unwrap_or(false)
    {
        return Err(YoutubeAudioDownloadError::OutputRejected);
    }
    let path = fs::canonicalize(path).map_err(|_| YoutubeAudioDownloadError::OutputRejected)?;
    let canonical_directory = fs::canonicalize(directory.path())
        .map_err(|_| YoutubeAudioDownloadError::OutputRejected)?;
    if !path.starts_with(canonical_directory)
        || fs::metadata(&path)
            .map(|metadata| metadata.len() > max_staged_bytes)
            .unwrap_or(true)
        || audio_format_from_path(&path).is_err()
    {
        return Err(YoutubeAudioDownloadError::OutputRejected);
    }

    Ok(StagedYoutubeAudio {
        _directory: directory,
        path,
    })
}

fn terminate_process_group(child: &mut Child) {
    let _ = kill_process_group(Pid::from_child(child), Signal::KILL);
    let _ = child.kill();
    let _ = child.wait();
}

fn staged_directory_size(
    directory: &Path,
    stop_after_bytes: u64,
) -> Result<u64, YoutubeAudioDownloadError> {
    let mut total = 0u64;
    let mut entry_count = 0usize;
    let mut pending = vec![directory.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let entries =
            fs::read_dir(directory).map_err(|_| YoutubeAudioDownloadError::OutputRejected)?;
        for entry in entries {
            let entry = entry.map_err(|_| YoutubeAudioDownloadError::OutputRejected)?;
            entry_count = entry_count.saturating_add(1);
            if entry_count > MAX_YOUTUBE_STAGED_ENTRIES {
                return Err(YoutubeAudioDownloadError::OutputRejected);
            }
            let file_type = entry
                .file_type()
                .map_err(|_| YoutubeAudioDownloadError::OutputRejected)?;
            if file_type.is_dir() {
                pending.push(entry.path());
            } else if file_type.is_file() {
                let metadata = entry
                    .metadata()
                    .map_err(|_| YoutubeAudioDownloadError::OutputRejected)?;
                total = total.saturating_add(metadata.len());
                if total > stop_after_bytes {
                    return Ok(total);
                }
            } else {
                return Err(YoutubeAudioDownloadError::OutputRejected);
            }
        }
    }
    Ok(total)
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
    use std::{
        os::unix::fs::PermissionsExt,
        sync::atomic::AtomicBool,
        time::{Duration, Instant},
    };

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

    #[test]
    fn oversized_intermediate_kills_child_and_removes_staging_directory() {
        let (fixture, executable, record) = fake_yt_dlp(
            r#"dd if=/dev/zero of="$directory/oversized.tmp" bs=1 count=256 2>/dev/null
sleep 5"#,
        );
        let shutdown = AtomicBool::new(false);

        let result = download_with_limits(
            "https://youtu.be/abc",
            &shutdown,
            &executable,
            Duration::from_secs(2),
            64,
        );

        assert!(fixture.path().exists());
        assert!(matches!(
            result,
            Err(YoutubeAudioDownloadError::PayloadTooLarge)
        ));
        assert!(!recorded_staging_directory(&record).exists());
    }

    #[test]
    fn timed_out_download_kills_child_and_removes_staging_directory() {
        let (_fixture, executable, record) = fake_yt_dlp("sleep 5");
        let shutdown = AtomicBool::new(false);
        let started_at = Instant::now();

        let result = download_with_limits(
            "https://youtu.be/abc",
            &shutdown,
            &executable,
            Duration::from_millis(20),
            1024,
        );

        assert!(
            matches!(result, Err(YoutubeAudioDownloadError::TimedOut)),
            "unexpected result: {result:?}"
        );
        assert!(started_at.elapsed() < Duration::from_secs(2));
        assert!(!recorded_staging_directory(&record).exists());
    }

    fn fake_yt_dlp(body: &str) -> (TempDir, PathBuf, PathBuf) {
        let fixture = tempfile::tempdir().expect("fixture directory");
        let executable = fixture.path().join("yt-dlp");
        let record = fixture.path().join("staging-directory.txt");
        let script = format!(
            r#"#!/bin/sh
previous=""
output=""
while [ "$#" -gt 0 ]; do
    if [ "$previous" = "--output" ]; then
        output="$1"
    fi
    previous="$1"
    shift
done
directory=$(dirname "$output")
printf "%s" "$directory" > "{}"
{}
"#,
            record.display(),
            body,
        );
        fs::write(&executable, script).expect("write fake yt-dlp");
        let mut permissions = fs::metadata(&executable)
            .expect("script metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&executable, permissions).expect("make fake yt-dlp executable");
        (fixture, executable, record)
    }

    fn recorded_staging_directory(record: &Path) -> PathBuf {
        PathBuf::from(fs::read_to_string(record).expect("recorded staging directory"))
    }
}
