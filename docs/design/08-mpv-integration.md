# 08 — mpv integration

Last updated: 2026-04-22

Playback happens in an external `mpv` process. The server spawns it, connects to its JSON IPC
socket, observes `time-pos`, and persists progress to `watch_history`.

## Spawning

On `POST /api/videos/:id/play`:

1. If an active session exists for `video_id`, terminate it (kill mpv, close socket).
2. Allocate `/tmp/vidviewer-mpv-<uuid>.sock`.
3. Determine start position:
   - If `?start=<secs>` query present, use it.
   - Else use `watch_history.position_secs` (if any, and not completed).
   - Else 0.
4. Spawn `mpv --input-ipc-server=<sock> --start=<start> --force-window=yes <abs_path>` plus
   any `player_args` from config.
5. Return 202 with `{ video_id, session_id }`.

## IPC session task

A `tokio` task per session:

1. Open the Unix socket, retrying for up to ~5 s (mpv takes a moment to create it).
2. Send `{"command": ["observe_property", 1, "time-pos"]}`.
3. Read newline-delimited JSON events.
4. Throttle DB writes: update `watch_history.position_secs` and `last_watched_at` every ~5 s.
5. Increment `watch_count` on the first event of the session.
6. On `end-file` or socket close:
   - If `position_secs / duration_secs >= 0.9`, set `completed = 1` and reset `position_secs` to 0.
   - Otherwise leave `position_secs` at last observed value.
7. Drop the session from the session map.

## Concurrency

- One active session per `video_id`.
- Multiple different videos can play simultaneously (each with its own socket + task).
- The session map is a `tokio::sync::Mutex<HashMap<VideoId, SessionHandle>>`.

## Failure modes

| Situation | Behavior |
|---|---|
| `mpv` not found on PATH | Spawn fails; API returns 500 with `mpv_not_found`; UI shows toast. `vidviewer doctor` also reports this. |
| Socket never appears | Retry up to ~5 s; then give up, log error; the mpv window may still play but progress isn't tracked. |
| Socket closes unexpectedly | Session task ends; last-known position persists. |
| mpv exits with nonzero | Session ends; position persists. |

## Player trait

All playback goes through `player::Player`:

```rust
pub trait Player: Send + Sync {
    async fn launch(
        &self,
        video: &Video,
        start_secs: f64,
    ) -> anyhow::Result<SessionHandle>;
}
```

Real: `MpvPlayer`. Tests: `MockPlayer` that records launches and can be driven synthetically.

See [`../agents/adding-a-page.md`](../agents/adding-a-page.md) for how the Play button
routes through this trait.
