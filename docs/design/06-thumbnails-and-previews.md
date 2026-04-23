# 06 — Thumbnails and previews

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
- `thumbnail_width` from config (default 480).
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
- `preview_target_count = 56`.

### Generation

Preview generation is a two-step process that bounds memory per ffmpeg invocation
to a single decoder context, regardless of how many previews a video has.

**Step 1 — per-timestamp extraction (serial).** For each timestamp in the preview
plan, a separate ffmpeg invocation uses input-side seek and writes a small JPEG to
a scratch directory:

```
ffmpeg -y -ss <T_i> -i <abs_path> \
    -frames:v 1 \
    -vf scale=160:90:force_original_aspect_ratio=decrease,pad=160:90:(ow-iw)/2:(oh-ih)/2:black \
    <scratch>/<i:03>.jpg
```

Scratch path: `<cache>/previews/scratch/<video_id>/<NNN>.jpg`. Cleaned up on
success and failure via an RAII guard in Rust.

If one per-timestamp extraction fails (malformed container, missing seek index for
a given spot, etc.), the scanner logs a warning and substitutes the previously
successful tile so the final tile sheet stays complete. A `partial_tiles` count is
logged at job end. If frame 0 fails and the immediate successor also fails, the
whole job fails.

**Step 2 — tile pass.** A single ffmpeg invocation reads the scratch frames via
the `image2` demuxer and assembles them:

```
ffmpeg -y -start_number 0 -framerate 1 -i <scratch>/%03d.jpg \
    -frames:v 1 -vframes <count> \
    -vf tile=<cols>x<rows> \
    <cache>/previews/<video_id>.jpg
```

One input, small JPEG frames → bounded memory regardless of tile count.

### Why this shape

An earlier attempt used a single ffmpeg with N `-ss T_i -i <src>` input pairs plus
`xstack`. That produced a correct tile sheet but kept N full decoder contexts
resident simultaneously and pinned buffered frames in `xstack` while it waited to
synchronize all inputs, driving memory into the tens of GB on long h264/hevc
videos. The split into N small processes + one tiling process bounds memory at
one decoder context at a time.

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
