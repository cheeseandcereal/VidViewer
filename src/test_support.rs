//! Test-support helpers. Not feature-gated because integration tests in
//! `tests/` link against the crate normally and can't access `#[cfg(test)]`
//! items. Only intended to be called from test code.

use std::path::Path;

/// Minimal MP4 `ftyp` box that `infer` recognizes as a video container.
///
///   00 00 00 20  ftyp  isom 00 00 02 00 isom iso2 avc1 mp41
///
/// `infer` only looks at magic bytes; we don't actually decode the box.
pub const MP4_FTYP_HEADER: &[u8] = &[
    0x00, 0x00, 0x00, 0x20, b'f', b't', b'y', b'p', b'i', b's', b'o', b'm', 0x00, 0x00, 0x02, 0x00,
    b'i', b's', b'o', b'm', b'i', b's', b'o', b'2', b'a', b'v', b'c', b'1', b'm', b'p', b'4', b'1',
];

/// Write a file in `dir` with a valid MP4 `ftyp` header followed by the
/// caller-supplied filler bytes. The scanner's content-sniff step will
/// classify the result as video.
pub fn write_video_fixture(dir: &Path, name: &str, filler: &[u8]) {
    let mut full = Vec::with_capacity(filler.len() + MP4_FTYP_HEADER.len());
    full.extend_from_slice(MP4_FTYP_HEADER);
    full.extend_from_slice(filler);
    std::fs::write(dir.join(name), full).unwrap();
}
