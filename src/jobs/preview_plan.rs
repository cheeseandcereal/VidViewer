//! Preview plan computation and WebVTT sidecar generation.
//!
//! See `docs/design/06-thumbnails-and-previews.md` for the spec.

use std::fmt::Write as _;

use crate::video_tool::PreviewPlan;

/// Default preview tile dimensions (pixels).
pub const TILE_WIDTH: u32 = 160;
pub const TILE_HEIGHT: u32 = 90;

pub struct PlanInput {
    pub duration_secs: f64,
    pub min_interval_secs: f64,
    pub target_count: u32,
}

/// Compute the plan for a video. Returns `None` if the video has zero duration.
pub fn plan(input: &PlanInput) -> Option<PreviewPlan> {
    if input.duration_secs <= 0.0 {
        return None;
    }
    let min_interval = input.min_interval_secs.max(0.1);
    let target = input.target_count.max(2);

    let count = if input.duration_secs < 2.0 * min_interval {
        // Ultra-short fallback: exactly two previews at 25% and 75%.
        2u32
    } else {
        let by_interval = (input.duration_secs / min_interval).floor() as u32;
        target.min(by_interval).max(2)
    };

    let (timestamps, interval) = if count == 2 && input.duration_secs < 2.0 * min_interval {
        let ts = vec![input.duration_secs * 0.25, input.duration_secs * 0.75];
        (ts, input.duration_secs / 2.0)
    } else {
        let interval = input.duration_secs / count as f64;
        let ts = (0..count).map(|i| (i as f64 + 0.5) * interval).collect();
        (ts, interval)
    };

    let cols = (count as f64).sqrt().ceil() as u32;
    let cols = cols.max(1);
    let rows = count.div_ceil(cols);

    Some(PreviewPlan {
        count,
        timestamps,
        cols,
        rows,
        tile_width: TILE_WIDTH,
        tile_height: TILE_HEIGHT,
    })
    .map(|mut p| {
        let _ = interval;
        // interval unused from here; kept for clarity above.
        p.timestamps.truncate(count as usize);
        p
    })
}

/// Render a WebVTT file whose cues reference rectangles within the sprite sheet at `sheet_url`.
/// `sheet_url` is used as-is in each cue; callers should add cache-busting query strings if needed.
pub fn render_vtt(plan: &PreviewPlan, sheet_url: &str, duration_secs: f64) -> String {
    let mut out = String::from("WEBVTT\n\n");
    for i in 0..plan.count {
        let col = i % plan.cols;
        let row = i / plan.cols;
        let x = col * plan.tile_width;
        let y = row * plan.tile_height;

        let start = plan.timestamps.get(i as usize).copied().unwrap_or(0.0)
            - (duration_secs / plan.count as f64) / 2.0;
        let end = plan.timestamps.get(i as usize).copied().unwrap_or(0.0)
            + (duration_secs / plan.count as f64) / 2.0;
        let start = start.max(0.0);
        let end = end.min(duration_secs);

        let _ = writeln!(
            out,
            "{}",
            format_vtt_time(start) + " --> " + &format_vtt_time(end)
        );
        let _ = writeln!(
            out,
            "{}#xywh={},{},{},{}",
            sheet_url, x, y, plan.tile_width, plan.tile_height
        );
        out.push('\n');
    }
    out
}

fn format_vtt_time(secs: f64) -> String {
    let total_ms = (secs * 1000.0).round() as i64;
    let total_ms = total_ms.max(0);
    let h = total_ms / 3_600_000;
    let m = (total_ms % 3_600_000) / 60_000;
    let s = (total_ms % 60_000) / 1000;
    let ms = total_ms % 1000;
    format!("{h:02}:{m:02}:{s:02}.{ms:03}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_video_gets_two_previews() {
        let p = plan(&PlanInput {
            duration_secs: 3.0,
            min_interval_secs: 2.0,
            target_count: 100,
        })
        .unwrap();
        assert_eq!(p.count, 2);
        assert!((p.timestamps[0] - 0.75).abs() < 1e-6);
        assert!((p.timestamps[1] - 2.25).abs() < 1e-6);
    }

    #[test]
    fn long_video_caps_at_target() {
        let p = plan(&PlanInput {
            duration_secs: 7200.0, // 2h
            min_interval_secs: 2.0,
            target_count: 100,
        })
        .unwrap();
        assert_eq!(p.count, 100);
        // Grid should be 10x10.
        assert_eq!(p.cols, 10);
        assert_eq!(p.rows, 10);
    }

    #[test]
    fn medium_video_respects_min_interval() {
        let p = plan(&PlanInput {
            duration_secs: 30.0,
            min_interval_secs: 2.0,
            target_count: 100,
        })
        .unwrap();
        assert_eq!(p.count, 15);
    }

    #[test]
    fn vtt_format() {
        let p = plan(&PlanInput {
            duration_secs: 10.0,
            min_interval_secs: 2.0,
            target_count: 100,
        })
        .unwrap();
        let vtt = render_vtt(&p, "/previews/abc.jpg", 10.0);
        assert!(vtt.starts_with("WEBVTT\n\n"));
        assert!(vtt.contains("#xywh=0,0,160,90"));
        assert!(vtt.contains("-->"));
    }

    #[test]
    fn zero_duration_returns_none() {
        assert!(plan(&PlanInput {
            duration_secs: 0.0,
            min_interval_secs: 2.0,
            target_count: 100,
        })
        .is_none());
    }
}
