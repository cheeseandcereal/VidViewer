//! Path helpers.
//!
//! On Linux, filenames are bytes that may not be valid UTF-8. We convert to `String` via
//! lossy conversion and log a warning when the conversion is actually lossy, so the user
//! can decide whether to rename the problematic file.

use std::path::Path;

/// Convert a path to `String` for DB storage. Emits a warning via `tracing` if the
/// conversion loses information (i.e. the path contained non-UTF-8 bytes).
pub fn path_to_db_string(path: &Path) -> String {
    let lossy = path.to_string_lossy();
    if path.as_os_str().as_encoded_bytes() != lossy.as_bytes() {
        tracing::warn!(
            path = %lossy,
            "filename contains non-UTF-8 bytes; the stored path may not round-trip exactly"
        );
    }
    lossy.into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn utf8_path_is_lossless() {
        let p = PathBuf::from("/tmp/漢字.mp4");
        assert_eq!(path_to_db_string(&p), "/tmp/漢字.mp4");
    }

    #[test]
    fn ascii_path_is_lossless() {
        let p = PathBuf::from("/tmp/plain.mp4");
        assert_eq!(path_to_db_string(&p), "/tmp/plain.mp4");
    }
}
