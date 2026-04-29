# VidViewer — Agent Guide

This file gives AI agents (and humans) the context needed to be productive on this codebase quickly.

## What this is

**VidViewer** is a local-first, self-hosted web app for browsing a personal media library.
It scans configured directories (media files are detected by content sniffing — no
extension allowlist), generates thumbnails and hover-scrub previews, organizes items
into collections (directory-based and custom), and launches selected files in `mpv`
(not in-browser). Watch history and resume positions are captured via `mpv`'s JSON IPC.

Both video and audio files are supported. Audio files skip preview generation and use
their embedded cover art (or a static music-note placeholder) as the thumbnail; mpv is
launched with `--force-window=yes` so audio playback still shows a controllable window.
Despite the historical "video" naming in the codebase (tables, types, routes), a
"video" row in VidViewer is any playable media item, audio included.

## Platform and stack

- **Target OS:** Linux only. Never add macOS or Windows support.
- **Language:** Rust stable (MSRV 1.82, declared via `rust-version` in `Cargo.toml`)
- **HTTP:** `axum` + `tower-http`
- **Templates:** `askama` (compile-time checked)
- **Database:** SQLite via `sqlx` (compile-time query checking in offline mode)
- **Runtime:** `tokio`
- **Logging:** `tracing` (pretty by default, JSON via `LOG_FORMAT=json`)
- **External tools (must be on `PATH`):** `ffmpeg`, `ffprobe`, `mpv`
- **Frontend:** hand-written CSS and vanilla JS. **No build step, ever.**

## Where to look first

| You want to... | Read this |
|---|---|
| Understand the project at a glance | `docs/design/01-overview.md` |
| Understand components and request flow | `docs/design/02-architecture.md` |
| Work with the DB schema | `docs/design/03-data-model.md` |
| Understand the scanner | `docs/design/04-scanner.md` |
| Add a job kind | `docs/agents/adding-a-job-kind.md` |
| Add a new page | `docs/agents/adding-a-page.md` |
| Change the schema | `docs/agents/changing-the-schema.md` |
| Debug a failing build/test/run | `docs/agents/debugging.md` |
| See what's left in the MVP | `docs/plan/mvp-build-order.md` |
| See post-MVP ideas | `docs/plan/deferred.md` |

## Conventions

### Commits
- Use Conventional Commits: `feat:`, `fix:`, `refactor:`, `docs:`, `test:`, `chore:`.
- Every commit should leave `just check && just test` passing.

### Code style
- Use `sqlx` with `query!` / `query_as!` macros (compile-time SQL verification).
  Keep `.sqlx/` metadata committed so offline compilation works.
- Use `tracing` for logs. Never `println!` or `eprintln!` in library code.
- Always pass filesystem paths via `Command::arg(&Path)`, never via shell strings.
- Always percent-encode text placed into URL paths or query strings. Helpers live in `src/util/url.rs`.
- Set `Content-Type: text/html; charset=utf-8` on all HTML responses (default via Askama integration, but assert explicitly in tests).
- Prefer `tokio::fs` over `std::fs` in request and job paths.
- Use `anyhow::Result` at boundaries; use typed errors (`thiserror`) where they cross module lines.
- No bare `?` on IO — add `.context("...")`.
- Use typed IDs: `VideoId`, `CollectionId`, `DirectoryId` (see `src/ids.rs`). Never pass bare `String`/`i64` IDs.
- Use the `Clock` trait (`src/clock.rs`) for any `now()` call. Inject it so tests are deterministic.
- Prefer straightforward imperative code over clever iterator chains.
- Target ≤ 400 LOC of production code per file; split earlier rather than
  later. Test modules are counted separately (see "Tests layout" below).

### External process boundaries
All code that shells out to ffmpeg/ffprobe/mpv must go through a trait:
- `Player` trait → `MpvPlayer` (real) / `MockPlayer` (tests). See `src/player/`.
- `VideoTool` trait → `FfmpegTool` (real) / `MockVideoTool` (tests). See `src/video_tool/`.

Tests should use the mocks unless they are specifically exercising the real binaries (behind an integration-test feature flag).

### Tests layout
- **Unit tests live inline at the bottom of the source file that owns
  the behavior they cover**, inside a `#[cfg(test)] mod tests { ... }`
  block. This is the standard Rust convention (see the Rust Book) and
  what this codebase uses everywhere. Benefits: tests sit next to the
  code they document, have free access to private items of the module,
  and rename/move together under refactors.
- **"Tests next to the code they test" is about the file, not the
  module directory.** When a module is split across sibling files
  (`foo/reads.rs`, `foo/mutations.rs`, `foo/types.rs`), each test
  block goes into the file whose functions it exercises — not a single
  dumping-ground block in `foo/mod.rs`. See `src/collections/` and
  `src/jobs/` for the pattern.
- **Do not create `src/<module>/tests.rs` sibling files.** Tests must
  be inline in their corresponding source file. If a test module grows
  uncomfortably large, that's usually a signal the *production* module
  should be split — extract sibling files by behavior and move each
  test block with its target.
- **Shared test fixtures within a module go in a
  `src/<module>/test_helpers.rs` file**, gated with
  `#![cfg(test)]` and exposing `pub(super) fn ...` helpers. This is a
  *helpers module*, not a tests module — it must not contain any
  `#[test]` or `#[tokio::test]` functions. See
  `src/jobs/test_helpers.rs`, `src/collections/test_helpers.rs`, and
  `src/http/api/test_helpers.rs` for the established pattern.
- **Cross-module shared fixtures** live in `src/test_support.rs`,
  exposed as `pub` because integration tests in the top-level
  `tests/` directory link against the crate normally and can't reach
  `#[cfg(test)]` items. Keep that module tiny and helper-only (the
  only item today is `write_video_fixture`).
- **Integration tests** (cross-module, testing the public surface
  end-to-end) go in the top-level `tests/` directory — one file per
  scenario, e.g. `tests/worker_pipeline.rs`, `tests/cancellation.rs`.
- **Target ≤ 400 LOC per source file for *production* code**, with
  tests counted separately. A file that's 300 lines of code + 500
  lines of tests is fine. A file that's 500 lines of production code
  is a signal to split.

### Schema migrations
- Migrations live in `migrations/NNNN_description.sql`.
- **Never edit a committed migration.** Always add a new one.
- When changing the schema, update `docs/design/03-data-model.md` in the same change.
- A backup is taken automatically before any migration runs at startup. See `docs/design/14-migrations.md`.

### Documentation
- **Any** design change must update the relevant `docs/design/*.md` in the same change.
- When a build step from `docs/plan/mvp-build-order.md` is completed, check it off.
- When new ideas come up that are out of MVP scope, add them to `docs/plan/deferred.md`.
- Per-subsystem README files live alongside their code (e.g. `src/player/README.md`) for local nuances that don't rise to the level of a design doc.

## Invariants

These must remain true; violating them means a bug:

1. `(directory_id, relative_path)` uniquely identifies a video row.
2. `video_id` is stable across file content changes — only the content hash (`size`, `mtime`) and derived assets change. URLs to thumbnails and previews are cache-busted via `?v=<updated_at_epoch>`.
3. Directory collections (`collections.kind = 'directory'`) can only be mutated via their `name` field. All other mutations must return `400` at the API boundary.
4. Collection membership is computed on read, never materialized. A directory collection's videos are those with `videos.directory_id = collections.directory_id`; a custom collection's videos are the union of videos in the directories listed in `collection_directories` for that collection. Videos flagged `missing = 1` are excluded from all listings.
5. Soft-removed directories (`directories.removed = 1`) are skipped by the scanner. Their directory collection is flagged `hidden = 1` and not shown in listings. Re-adding the same path un-hides and un-removes in place. Any `collection_directories` rows linking a soft-removed directory to a custom collection remain intact, so re-adding the directory restores its contribution to those collections automatically.
6. Watch history (`watch_history` rows) is preserved under **soft remove**. **Hard
   remove** of a directory (user-confirmed destructive action) cascades through
   `videos` → `watch_history`, the directory's `collections` row, and any
   `collection_directories` rows referencing that directory, and also deletes
   cached thumbnails and previews for those videos from disk.

## What not to do

- Do not add Windows or macOS support.
- Do not add a frontend build step (no npm, webpack, vite, etc.).
- Do not add auth or LAN exposure — this is localhost-only.
- Do not add in-browser playback in v1.
- Do not edit committed migrations.
- Directory removal is either soft (default, preserves history) or hard
  (explicit user action, cascades via FK). Never perform a hard-remove as a
  side effect — it must be user-initiated through the `mode=hard` API path.

## Running and testing

| Task | Command |
|---|---|
| Format | `just fmt` |
| Lint | `just lint` |
| Type-check | `just check` |
| Unit + integration tests | `just test` |
| Line coverage report (HTML) | `just coverage` |
| Run the server | `just run` |
| Environment sanity | `just doctor` |
| End-to-end smoke | `just smoke` |
| Refresh sqlx offline metadata | `just prepare-sqlx` |

## Test coverage

We track line/region/function coverage via [`cargo-llvm-cov`](https://github.com/taiki-e/cargo-llvm-cov).

- Run `just coverage`. It produces a per-file summary on stdout and writes an
  HTML report to `target/llvm-cov/html/index.html`.
- Prerequisites (one-time):
  - `rustup component add llvm-tools-preview`
  - `cargo install cargo-llvm-cov --locked`
  The `just coverage` recipe resolves the toolchain-specific `llvm-cov` /
  `llvm-profdata` paths dynamically, so it works on whichever stable
  toolchain is active without extra env-var plumbing.
- **Expected overall coverage is ~85%** after the existing test suite.
  New code should aim for a similar or better ratio on the files it
  touches. Deliberate gaps (see below) are fine; contrived tests to hit
  lines are not.

Files that are intentionally under-covered, and the reason:

- `src/main.rs` — binary entrypoint.
- `src/logging.rs` — tracing subscriber init.
- `src/cli.rs` — top-level clap dispatch and doctor subcommand; exercised
  by running the binary (`just doctor`, `just smoke`), not by unit tests.
- `src/player/session.rs` — the mpv JSON-IPC session loop. Testing the
  real loop requires a real mpv or a socket mock that reimplements the
  protocol. Covered by manual use of the app; `MockPlayer` is used in
  tests.
- `src/video_tool/ffmpeg.rs` async methods — the real `FfmpegTool`
  invocations. Tests go through `MockVideoTool`; the command-builder
  free functions (e.g. `build_single_frame_command`) are unit-tested.
- `src/clock.rs` fake-clock helper — unused by tests today; exists for
  future deterministic-time tests.

When adding new code, prefer **real behavior assertions** (status code,
DB row contents, stored side-effects) over line-hitting assertions.
Router-level tests using `tower::util::ServiceExt::oneshot` against the
real router with `AppState::for_test` are the established pattern (see
`src/http/api.rs::tests` for the model).

## How to verify your changes

Before considering a change complete, run:
```
just fmt
just lint
just check
just test
```
All four must pass.

If you changed behavior under test, run `just coverage` and confirm no
unexpected coverage regressions on the files you touched.

If you changed the schema, also:
- Confirm a new migration file was added (not an edit to an existing one).
- Run `just prepare-sqlx` and commit the updated `.sqlx/` metadata.
- Update `docs/design/03-data-model.md`.

If you changed the HTTP API, update `docs/design/09-http-api.md`.

If you changed behavior described in any `docs/design/*.md` file, update that file.
