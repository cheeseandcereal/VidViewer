# MVP build order

All MVP steps are complete.

- [x] **Step 0** — Documentation, tooling, conventions
- [x] **Step 1** — Cargo scaffold, config, tracing, Axum `/healthz`
- [x] **Step 2** — Storage foundation: SQLite pool, `Clock`, typed IDs, migrations, pre-migration backup, integrity check
- [x] **Step 3** — UTF-8 hygiene: lossy-filename warning, URL percent-encoding, CJK font stack, base template
- [x] **Step 4** — Directories CRUD + Settings page + directory picker modal
- [x] **Step 5** — Scanner with directory-collection materialization + soft-delete
- [x] **Step 6** — `Player` + `VideoTool` traits (real + mock)
- [x] **Step 7** — Collections API + Home + Collection page + Random endpoint + `R` hotkey
- [x] **Step 8** — Probe + thumbnail jobs + `/thumbs/:id.jpg`
- [x] **Step 9** — Preview jobs (tile sheet + WebVTT) + `/previews/*`
- [x] **Step 10** — Hover-scrub JS component
- [x] **Step 11** — Video detail page
- [x] **Step 12** — mpv launch + IPC session + history + resume + `?start=`
- [x] **Step 13** — Random navigation + Re-roll on detail page
- [x] **Step 14** — Custom collections CRUD + add/remove videos
- [x] **Step 15** — History page with resume progress
- [x] **Step 16** — Polish:
  - Scan progress UI with polling
  - Missing badges in grid
  - Cache-busting via `?v=<updated_at_epoch>`
  - Empty states on all pages
  - `vidviewer doctor` subcommand
  - Gated `/debug` endpoint
  - Top-level README.md

## Post-MVP

See [`deferred.md`](./deferred.md).
