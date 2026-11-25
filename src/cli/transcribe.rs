//! `cataly transcribe <file|url>` (VOICE-24, OJ-32; VOICE req. 86): runs the owner's configured
//! command on an audio file and prints its trimmed output.
//!
//! A URL is downloaded into a private temporary directory under the state directory through the one
//! HTTP download (`relay::http::download`, at most 5 redirects, 60 s in all), capped at the
//! configured size (25 MiB by default) of actual bytes; a larger declared
//! size, an empty body, or leading bytes of no known audio container refuse it. The download is
//! deleted afterwards. A non-zero exit or empty output is an error with at most 160 characters of
//! error text. Transcription runs on request only (`DESIGN.md` 12.2, A22).

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::voice::TranscribeSettings;
use crate::relay::http;
use crate::sys::command::{self, Output, Spec};

/// The transcription command may run this long.
const RUN: Duration = Duration::from_mins(10);
/// A URL download may take this long, redirects included (MSG req. 9).
const DOWNLOAD: Duration = Duration::from_secs(60);
/// Captured output per stream.
const OUTPUT_CAP: u64 = 1024 * 1024;
/// Error text kept.
const ERROR_MAX: usize = 160;

/// Known audio containers by their leading bytes: Ogg (Opus voice notes), RIFF/WAVE, FLAC, MP3
/// (ID3 or a frame sync), MP4/M4A (`ftyp` box), WebM/Matroska (EBML), AMR, CAF.
pub(crate) fn audio_container(head: &[u8]) -> Option<&'static str> {
    let at = |o: usize, sig: &[u8]| head.get(o..o + sig.len()) == Some(sig);
    if at(0, b"OggS") {
        Some("ogg")
    } else if at(0, b"RIFF") && at(8, b"WAVE") {
        Some("wav")
    } else if at(0, b"fLaC") {
        Some("flac")
    } else if at(0, b"ID3")
        || (head.first() == Some(&0xFF) && head.get(1).is_some_and(|b| b & 0xE0 == 0xE0))
    {
        Some("mp3")
    } else if at(4, b"ftyp") {
        Some("m4a")
    } else if at(0, &[0x1A, 0x45, 0xDF, 0xA3]) {
        Some("webm")
    } else if at(0, b"#!AMR") {
        Some("amr")
    } else if at(0, b"caff") {
        Some("caf")
    } else {
        None
    }
}

fn short(text: &str) -> String {
    let one: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    one.chars().take(ERROR_MAX).collect()
}

/// Downloads `url` into `dir`, refusing what VOICE req. 86 refuses.
async fn download(url: &str, dir: &Path, cap: u64) -> Result<PathBuf, String> {
    let cap = usize::try_from(cap).unwrap_or(usize::MAX);
    let body = tokio::time::timeout(DOWNLOAD, http::download(url, cap))
        .await
        .map_err(|_| "download failed: no complete download within 60 s".to_string())?
        .map_err(|e| format!("download failed: {e}"))?;
    if body.is_empty() {
        return Err("the download is empty".into());
    }
    let Some(kind) = audio_container(&body) else {
        return Err("the download is not a known audio container".into());
    };
    let file = dir.join(format!("audio.{kind}"));
    crate::sys::fs::create_atomic(&file, &body).map_err(|e| e.to_string())?;
    Ok(file)
}

/// Runs the command on `target` (a path or an `http(s)://` URL); returns the transcript.
pub(crate) async fn transcribe(
    settings: &TranscribeSettings,
    target: &str,
    scratch: &Path,
) -> Result<String, String> {
    if settings.command.is_empty() {
        return Err("no transcription command: set [transcribe] command".into());
    }
    let is_url = target.starts_with("http://") || target.starts_with("https://");
    let dir = scratch.join(format!("transcribe-{}", crate::host::random_hex(8)));
    let file = if is_url {
        crate::sys::fs::ensure_private_dir(&dir).map_err(|e| e.to_string())?;
        match download(target, &dir, settings.max_bytes).await {
            Ok(f) => f,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&dir);
                return Err(e);
            }
        }
    } else {
        let path = PathBuf::from(target);
        if !path.is_file() {
            return Err(format!("no such file: {target}"));
        }
        path
    };
    let mut argv = settings.command.clone();
    argv.push(file.display().to_string());
    let spec = Spec {
        argv: &argv,
        cwd: None,
        env: &[],
        timeout: RUN,
        kill_grace: Duration::from_secs(5),
        output_cap: OUTPUT_CAP,
        output: Output::Capture,
    };
    let result = command::run(&spec).await;
    if is_url {
        let _ = std::fs::remove_dir_all(&dir);
    }
    let finished = result.map_err(|e| short(&e.to_string()))?;
    let out = String::from_utf8_lossy(&finished.stdout).trim().to_string();
    if !finished.success() {
        let err = String::from_utf8_lossy(&finished.stderr).to_string();
        return Err(short(&format!("the transcription command failed: {err}")));
    }
    if out.is_empty() {
        return Err("the transcription command printed nothing".into());
    }
    Ok(out)
}

/// The verb: `cataly transcribe <file|url>`.
pub(crate) fn run(words: &[String], paths: &crate::sys::paths::Paths) -> u8 {
    let [target] = words else {
        super::out::error_line("usage: cataly transcribe <file|url>");
        return super::exit::USAGE;
    };
    let config = match crate::config::load(&paths.config_file) {
        Ok(c) => c,
        Err(e) => {
            super::out::error_line(&format!("cataly: {}", crate::config::describe(&e)));
            return super::exit::FAILED;
        }
    };
    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        super::out::error_line("cataly: no async runtime");
        return super::exit::FAILED;
    };
    match rt.block_on(transcribe(&config.transcribe, target, &paths.files_dir)) {
        Ok(text) => {
            super::out::text(&text);
            super::exit::OK
        }
        Err(e) => {
            super::out::error_line(&format!("cataly: {e}"));
            super::exit::FAILED
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys::fs::tests::{make_executable, scratch};

    #[test]
    fn containers_by_leading_bytes() {
        assert_eq!(audio_container(b"OggS\0\x02"), Some("ogg"));
        assert_eq!(audio_container(b"RIFF\0\0\0\0WAVEfmt "), Some("wav"));
        assert_eq!(audio_container(b"\0\0\0\x20ftypM4A "), Some("m4a"));
        assert_eq!(audio_container(b"<html>"), None);
        assert_eq!(audio_container(b""), None);
    }

    struct Dir<'a>(&'a Path);
    impl Dir<'_> {
        fn path(&self) -> &Path {
            self.0
        }
    }

    /// Check 7: the owner's command runs on the file; failures are short errors; URLs obey the cap
    /// and the container check and are deleted afterwards.
    #[tokio::test]
    async fn runs_the_owner_command_with_cap_and_container_check() {
        let guard = scratch("transcribe");
        let dir = Dir(&guard);
        let tool = dir.path().join("stt");
        std::fs::write(&tool, "#!/bin/sh\nif [ \"$1\" = fail ]; then echo boom >&2; exit 3; fi\nhead -c 4 \"$2\"; echo ' heard'\n").unwrap();
        make_executable(&tool);
        let audio = dir.path().join("note.ogg");
        std::fs::write(&audio, b"OggS-voice-note").unwrap();
        let ok = TranscribeSettings {
            command: vec![tool.display().to_string(), "ok".into()],
            max_bytes: 1024,
        };
        let text = transcribe(&ok, &audio.display().to_string(), dir.path())
            .await
            .unwrap();
        assert_eq!(text, "OggS heard");
        let fail = TranscribeSettings {
            command: vec![tool.display().to_string(), "fail".into()],
            max_bytes: 1024,
        };
        let e = transcribe(&fail, &audio.display().to_string(), dir.path())
            .await
            .unwrap_err();
        assert!(e.contains("boom") && e.len() <= ERROR_MAX);

        // URLs: one fake server answering an audio body, an HTML body and an oversized body.
        let (base, _) = http::tests::canned(vec![
            "HTTP/1.1 200 OK\r\ncontent-length: 8\r\nconnection: close\r\n\r\nOggSdata",
            "HTTP/1.1 200 OK\r\ncontent-length: 6\r\nconnection: close\r\n\r\n<html>",
            "HTTP/1.1 200 OK\r\ncontent-length: 4096\r\nconnection: close\r\n\r\nOggS",
        ])
        .await;
        let url = format!("http://{}/note", base.authority());
        let text = transcribe(&ok, &url, dir.path()).await.unwrap();
        assert_eq!(text, "OggS heard");
        let e = transcribe(&ok, &url, dir.path()).await.unwrap_err();
        assert!(e.contains("not a known audio container"), "{e}");
        let e = transcribe(&ok, &url, dir.path()).await.unwrap_err();
        assert!(e.contains("1024"), "{e}");
        let left: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with("transcribe-"))
            .collect();
        assert!(left.is_empty(), "downloads are deleted");
    }
}
