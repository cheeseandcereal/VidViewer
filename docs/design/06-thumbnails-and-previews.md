# 06 — Thumbnails and previews

Last updated: 2026-04-22

Two derived asset kinds per video:

- **Thumbnail**: single poster JPEG used in grid cards and the detail page.
- **Preview**: tile sheet JPEG + WebVTT sidecar used for hover-scrub.

Both live under `~/.local/share/vidviewer/cache/`.

## Thumbnail

### Generation

```
ffmpeg -ss <T> -i <abs_path> -vframes 1 -vf scale=<thumbnail_width>:-1 <cache>/thumbs/<video_id>.jpg
```

- `T = duration_secs * 0.5` (midpoint of the video). Falls back to `5s` if the duration is
  unknown. Seek before input for speed.
- `thumbnail_width` from config (default 320).
- On success, set `videos.thumbnail_ok = 1`.

### Serving

- `GET /thumbs/:id.jpg` serves the cached file with long cache headers.
- URLs include `?v=<updated_at_epoch>` to bust caches when the file regenerates.

## Preview (hover-scrub)

### Distribution

Previews are distributed evenly across the full duration:

```
count = min(preview_target_count, floor(duration / preview_min_interval))
if duration < 2 * preview_min_interval:
    count = 2          # fallback at 25% and 75%
interval = duration / count
timestamps = [(i + 0.5) * interval for i in 0..count]
```

Tile dimensions are hardcoded: 160 × 90 per preview frame.
Grid dimensions are auto-computed: `cols = ceil(sqrt(count))`, `rows = ceil(count / cols)`.

Defaults:

- `preview_min_interval = 2` seconds.
- `preview_target_count = 100`.

### Generation

A single ffmpeg invocation producing one JPEG tile sheet:

```
ffmpeg -i <abs_path> \
    -vf "fps=<count>/<duration>,scale=160:90:force_original_aspect_ratio=decrease,pad=160:90:(ow-iw)/2:(oh-ih)/2:black,tile=<cols>x<rows>" \
    -frames:v 1 <cache>/previews/<video_id>.jpg
```

For short videos where count is forced to 2 at 25%/75%, the two timestamps are selected
with a `select` filter instead of `fps`, then tiled 2×1.

On success, set `videos.preview_ok = 1`.

### WebVTT sidecar

Generated in Rust after the tile sheet succeeds. Example format:

```
WEBVTT

00:00:00.000 --> 00:00:02.000
/previews/<video_id>.jpg#xywh=0,0,160,90

00:00:02.000 --> 00:00:04.000
/previews/<video_id>.jpg#xywh=160,0,160,90
```

- Each cue has `xywh` sprite coordinates.
- Cue duration = `interval`.
- The sheet URL includes the cache-bust query, so the VTT must be regenerated when the tile sheet regenerates.

### Serving

- `GET /previews/:id.jpg` — the tile sheet.
- `GET /previews/:id.vtt` — the sidecar.

## Hover-scrub behavior

See [`10-ui.md`](./10-ui.md). A shared vanilla-JS module maps mouse X position over a
card / poster to a cue in the VTT and renders the frame rectangle inline.

## Videos with unknown duration

If `duration_secs` is null (probe failed or reported 0):

- Preview generation is skipped; `preview_ok` stays 0.
- Hover-scrub degrades to a static poster.
