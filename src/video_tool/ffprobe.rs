//! ffprobe JSON output parser. Self-contained so it can be tested independently
//! without spawning any process.

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::video_tool::ProbeResult;

#[derive(Debug, Deserialize)]
struct FfprobeJson {
    format: Option<Format>,
    streams: Vec<Stream>,
}

#[derive(Debug, Deserialize)]
struct Format {
    duration: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Stream {
    codec_type: Option<String>,
    codec_name: Option<String>,
    width: Option<i64>,
    height: Option<i64>,
    duration: Option<String>,
}

pub(super) fn parse_ffprobe_json(bytes: &[u8]) -> Result<ProbeResult> {
    let parsed: FfprobeJson =
        serde_json::from_slice(bytes).context("parsing ffprobe json output")?;
    let video_stream = parsed
        .streams
        .iter()
        .find(|s| s.codec_type.as_deref() == Some("video"));

    let duration = parsed
        .format
        .as_ref()
        .and_then(|f| f.duration.as_deref())
        .and_then(|s| s.parse::<f64>().ok())
        .or_else(|| {
            video_stream
                .and_then(|s| s.duration.as_deref())
                .and_then(|s| s.parse::<f64>().ok())
        });

    Ok(ProbeResult {
        duration_secs: duration,
        width: video_stream.and_then(|s| s.width),
        height: video_stream.and_then(|s| s.height),
        codec: video_stream.and_then(|s| s.codec_name.clone()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ffprobe_duration_and_stream() {
        let sample = br#"{
            "format": {"duration": "123.456"},
            "streams": [
                {"codec_type":"audio","codec_name":"aac"},
                {"codec_type":"video","codec_name":"h264","width":1920,"height":1080}
            ]
        }"#;
        let r = parse_ffprobe_json(sample).unwrap();
        assert!((r.duration_secs.unwrap() - 123.456).abs() < 1e-6);
        assert_eq!(r.width, Some(1920));
        assert_eq!(r.height, Some(1080));
        assert_eq!(r.codec.as_deref(), Some("h264"));
    }
}
