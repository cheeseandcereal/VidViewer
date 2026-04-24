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
    #[serde(default)]
    disposition: Option<Disposition>,
}

#[derive(Debug, Deserialize, Default)]
struct Disposition {
    #[serde(default)]
    attached_pic: i64,
}

impl Stream {
    fn is_attached_pic(&self) -> bool {
        self.disposition
            .as_ref()
            .map(|d| d.attached_pic != 0)
            .unwrap_or(false)
    }
    fn is_video(&self) -> bool {
        self.codec_type.as_deref() == Some("video")
    }
    fn is_audio(&self) -> bool {
        self.codec_type.as_deref() == Some("audio")
    }
}

pub(super) fn parse_ffprobe_json(bytes: &[u8]) -> Result<ProbeResult> {
    let parsed: FfprobeJson =
        serde_json::from_slice(bytes).context("parsing ffprobe json output")?;

    // A stream is a "real" video stream if it's codec_type=video AND not
    // flagged as an attached picture (embedded cover art, which ffprobe
    // reports as a video stream).
    let real_video_stream = parsed
        .streams
        .iter()
        .enumerate()
        .find(|(_, s)| s.is_video() && !s.is_attached_pic())
        .map(|(_, s)| s);

    // First audio stream, if any.
    let audio_stream = parsed.streams.iter().find(|s| s.is_audio());

    // First attached-pic stream and its index, for cover art extraction.
    let attached_pic_stream_index = parsed.streams.iter().enumerate().find_map(|(i, s)| {
        if s.is_attached_pic() {
            Some(i as i64)
        } else {
            None
        }
    });

    let is_audio_only = audio_stream.is_some() && real_video_stream.is_none();

    let duration = parsed
        .format
        .as_ref()
        .and_then(|f| f.duration.as_deref())
        .and_then(|s| s.parse::<f64>().ok())
        .or_else(|| {
            // For audio-only files, fall back to the audio stream's own
            // duration; for real-video files, the video stream's duration.
            let stream = if is_audio_only {
                audio_stream
            } else {
                real_video_stream
            };
            stream
                .and_then(|s| s.duration.as_deref())
                .and_then(|s| s.parse::<f64>().ok())
        });

    // Codec: prefer the audio stream's codec when audio-only (so the detail
    // page shows "aac" / "flac" / etc. rather than the cover-art codec
    // "mjpeg"), otherwise the real video stream's codec.
    let codec = if is_audio_only {
        audio_stream.and_then(|s| s.codec_name.clone())
    } else {
        real_video_stream.and_then(|s| s.codec_name.clone())
    };

    Ok(ProbeResult {
        duration_secs: duration,
        width: real_video_stream.and_then(|s| s.width),
        height: real_video_stream.and_then(|s| s.height),
        codec,
        is_audio_only,
        attached_pic_stream_index,
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
        assert!(!r.is_audio_only);
        assert_eq!(r.attached_pic_stream_index, None);
    }

    #[test]
    fn audio_only_file_has_no_video_streams() {
        let sample = br#"{
            "format": {"duration": "201.0"},
            "streams": [
                {"codec_type":"audio","codec_name":"flac","duration":"201.0"}
            ]
        }"#;
        let r = parse_ffprobe_json(sample).unwrap();
        assert!(r.is_audio_only);
        assert_eq!(r.attached_pic_stream_index, None);
        assert_eq!(r.width, None);
        assert_eq!(r.height, None);
        assert_eq!(r.codec.as_deref(), Some("flac"));
        assert!((r.duration_secs.unwrap() - 201.0).abs() < 1e-6);
    }

    #[test]
    fn attached_pic_is_audio_only_with_cover_art_index() {
        // MP3/FLAC with embedded cover art: the cover is reported as a
        // video stream with disposition.attached_pic = 1.
        let sample = br#"{
            "format": {"duration": "180.0"},
            "streams": [
                {"codec_type":"audio","codec_name":"mp3"},
                {"codec_type":"video","codec_name":"mjpeg",
                 "width":500,"height":500,
                 "disposition":{"attached_pic":1}}
            ]
        }"#;
        let r = parse_ffprobe_json(sample).unwrap();
        assert!(r.is_audio_only);
        assert_eq!(r.attached_pic_stream_index, Some(1));
        // Width/height come from *real* video streams; attached pics don't count.
        assert_eq!(r.width, None);
        assert_eq!(r.height, None);
        // Codec should prefer the audio stream, not "mjpeg".
        assert_eq!(r.codec.as_deref(), Some("mp3"));
    }

    #[test]
    fn real_video_plus_attached_pic_is_not_audio_only() {
        // Pathological but possible: a movie file with embedded cover art.
        let sample = br#"{
            "format": {"duration": "7200.0"},
            "streams": [
                {"codec_type":"video","codec_name":"h264","width":1920,"height":1080},
                {"codec_type":"audio","codec_name":"aac"},
                {"codec_type":"video","codec_name":"mjpeg","disposition":{"attached_pic":1}}
            ]
        }"#;
        let r = parse_ffprobe_json(sample).unwrap();
        assert!(!r.is_audio_only);
        assert_eq!(r.attached_pic_stream_index, Some(2));
        assert_eq!(r.width, Some(1920));
        assert_eq!(r.codec.as_deref(), Some("h264"));
    }
}
