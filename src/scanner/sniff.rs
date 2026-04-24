//! Media-file detection by magic-byte content sniffing.
//!
//! The scanner calls `looks_like_media` for every file it considers
//! indexing. This replaces extension-based filtering: a user can drop
//! `song.bin` or `movie.unknown` into a configured directory and it will be
//! picked up correctly, and a `video.mp4` that's actually a text log will be
//! rejected.
//!
//! Only called for new files and files whose `(size, mtime)` changed since
//! the last scan — see `walk::scan_one`. Unchanged files skip sniffing; once
//! we've decided a row is media, we don't re-check its bytes on every scan.

use std::{fs::File, io::Read, path::Path};

use anyhow::{Context, Result};

/// 4096 bytes matches the typical filesystem block size on Linux (and the
/// native sector size of modern Advanced Format HDDs), so the sniff is a
/// single block read. It's well past what every `infer` audio/video matcher
/// actually inspects — the deepest matchers (MP4/M4A `ftyp` box, MPEG-TS
/// 188-byte packets) only look at the first few hundred bytes — with
/// comfortable headroom for any matcher upstream might add.
const SNIFF_BUFFER_BYTES: usize = 4096;

/// Returns `true` iff the first few KB of this file match a known audio or
/// video container signature. Returns `false` for every other classification
/// (`infer::MatcherType::Image`, `Archive`, `Doc`, …) and for files whose
/// content matches nothing.
///
/// I/O errors (unreadable file, permission denied, mid-read failure) are
/// surfaced to the caller so the walk phase can log a warning and move on
/// to the next entry, same as it does for stat failures.
pub(super) fn looks_like_media(path: &Path) -> Result<bool> {
    let mut buf = [0u8; SNIFF_BUFFER_BYTES];
    let n = read_header(path, &mut buf).with_context(|| format!("sniffing {}", path.display()))?;
    if n == 0 {
        return Ok(false);
    }
    Ok(is_media_buffer(&buf[..n]))
}

fn read_header(path: &Path, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut f = File::open(path)?;
    // Read up to buf.len() bytes. A single `read()` may return short even
    // on a large file, so loop until the buffer is full or EOF / zero.
    let mut filled = 0;
    while filled < buf.len() {
        match f.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}

fn is_media_buffer(buf: &[u8]) -> bool {
    match infer::get(buf) {
        Some(kind) => matches!(
            kind.matcher_type(),
            infer::MatcherType::Audio | infer::MatcherType::Video
        ),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_buffer_is_not_media() {
        assert!(!is_media_buffer(&[]));
    }

    #[test]
    fn short_text_is_not_media() {
        assert!(!is_media_buffer(b"skip"));
    }

    #[test]
    fn plaintext_is_not_media() {
        assert!(!is_media_buffer(
            b"This is a plain text log file with nothing media-shaped in it.\n"
        ));
    }

    #[test]
    fn mp3_id3v2_header_is_media() {
        // ID3v2 tag header: "ID3" + version(2) + flags(1) + size(4).
        let mut buf = Vec::new();
        buf.extend_from_slice(b"ID3\x04\x00\x00\x00\x00\x00\x00");
        // Pad with zeros to look plausible.
        buf.resize(256, 0);
        assert!(is_media_buffer(&buf));
    }

    #[test]
    fn flac_header_is_media() {
        // FLAC files begin with "fLaC".
        let mut buf = Vec::new();
        buf.extend_from_slice(b"fLaC");
        buf.resize(256, 0);
        assert!(is_media_buffer(&buf));
    }

    #[test]
    fn wav_riff_header_is_media() {
        // RIFF header for WAV: "RIFF" + size(4) + "WAVE" + ...
        let mut buf = Vec::new();
        buf.extend_from_slice(b"RIFF\x00\x00\x00\x00WAVEfmt ");
        buf.resize(64, 0);
        assert!(is_media_buffer(&buf));
    }

    #[test]
    fn matroska_ebml_header_is_media() {
        // Matroska / WebM start with the EBML magic 1A 45 DF A3.
        let mut buf = Vec::new();
        buf.extend_from_slice(&[0x1A, 0x45, 0xDF, 0xA3]);
        // A plausible EBML DocType "matroska" a bit further in.
        buf.extend_from_slice(&[0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
        buf.extend_from_slice(b"matroska");
        buf.resize(256, 0);
        assert!(is_media_buffer(&buf));
    }

    #[test]
    fn jpeg_is_not_media() {
        // JPEG SOI marker — infer classifies this as Image, not Audio/Video.
        let mut buf = Vec::new();
        buf.extend_from_slice(&[0xFF, 0xD8, 0xFF, 0xE0]);
        buf.resize(256, 0);
        assert!(!is_media_buffer(&buf));
    }

    #[test]
    fn short_file_with_valid_header_is_media() {
        // Files smaller than SNIFF_BUFFER_BYTES should still sniff correctly;
        // infer's matchers are header-based.
        let buf = b"fLaC\x00\x00\x00\x22some flac-ish bytes";
        assert!(is_media_buffer(buf));
    }
}
