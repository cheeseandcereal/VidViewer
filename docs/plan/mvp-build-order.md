# MVP build order

Last updated: 2026-04-22

This is the living task list for the MVP. Check items off as they land.

- [x] **Step 0** — Documentation, tooling, conventions
  - `AGENTS.md`
  - `rust-toolchain.toml`, `justfile`, `rustfmt.toml`, `clippy.toml`
  - `docs/README.md` + design docs + agent playbooks
  - `docs/plan/mvp-build-order.md` + `docs/plan/deferred.md`
  - `migrations/README.md`
- [ ] **Step 1** — Cargo scaffold
  - Dependencies, config loader, `tracing` init (pretty + JSON), Axum `/healthz`.
- [ ] **Step 2** — Storage foundation
  - `Clock` trait; `VideoId`/`CollectionId`/`DirectoryId` newtypes.
  - SQLite pool, `PRAGMA encoding`, migrations runner.
  - Pre-migration `VACUUM INTO` backup, keep-all retention.
  - Initial migration with all MVP tables.
  - Startup integrity check.
- [ ] **Step 3** — UTF-8 hygiene
  - Lossy-filename warning helper, percent-encoding helper.
  - CJK font stack in `templates/base.html`.
  - Assert `Content-Type: text/html; charset=utf-8` on all HTML.
- [ ] **Step 4** — Directories CRUD + Settings page + directory picker modal
- [ ] **Step 5** — Scanner with directory-collection materialization
- [ ] **Step 6** — `Player` + `VideoTool` traits (real + mock)
- [ ] **Step 7** — Collections API + Home + Collection page shell + Random endpoint
- [ ] **Step 8** — Probe + thumbnail jobs + `/thumbs/:id.jpg`
- [ ] **Step 9** — Preview jobs + `/previews/*`
- [ ] **Step 10** — Hover-scrub JS component
- [ ] **Step 11** — Video detail page
- [ ] **Step 12** — mpv launch + IPC + history + resume
- [ ] **Step 13** — Random navigation + `R` hotkey + Re-roll
- [ ] **Step 14** — Custom collections CRUD + add/remove videos
- [ ] **Step 15** — History page
- [ ] **Step 16** — Polish
  - Scan progress UI, missing badges, cache-busting, empty states.
  - `vidviewer doctor` subcommand.
  - Gated `/debug` endpoint.
  - `just smoke` end-to-end.
