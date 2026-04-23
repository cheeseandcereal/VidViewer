# VidViewer Documentation

This directory is the reference for how VidViewer is designed, built, and evolved.

## Layout

- [`design/`](./design/) — living reference documentation describing how each subsystem works.
- [`agents/`](./agents/) — task-specific playbooks for AI agents (and humans) performing common operations.
- [`plan/`](./plan/) — the living build plan and deferred feature list.

## Design docs

| # | File | Topic |
|---|------|-------|
| 01 | [`design/01-overview.md`](./design/01-overview.md) | Goals, scope, non-goals |
| 02 | [`design/02-architecture.md`](./design/02-architecture.md) | Components, request flow, process model |
| 03 | [`design/03-data-model.md`](./design/03-data-model.md) | SQLite schema and rationale |
| 04 | [`design/04-scanner.md`](./design/04-scanner.md) | Detection algorithm, missing handling |
| 05 | [`design/05-jobs-and-workers.md`](./design/05-jobs-and-workers.md) | Job queue, lanes, retry |
| 06 | [`design/06-thumbnails-and-previews.md`](./design/06-thumbnails-and-previews.md) | ffmpeg commands, preview distribution, WebVTT |
| 07 | [`design/07-collections.md`](./design/07-collections.md) | Directory vs custom collections |
| 08 | [`design/08-mpv-integration.md`](./design/08-mpv-integration.md) | IPC, resume, session lifecycle |
| 09 | [`design/09-http-api.md`](./design/09-http-api.md) | Routes, request/response shapes |
| 10 | [`design/10-ui.md`](./design/10-ui.md) | Pages, components, hotkeys |
| 11 | [`design/11-utf8-and-i18n.md`](./design/11-utf8-and-i18n.md) | UTF-8 correctness, CJK fonts |
| 12 | [`design/12-configuration.md`](./design/12-configuration.md) | `config.toml` reference |
| 14 | [`design/14-migrations.md`](./design/14-migrations.md) | Migration rules, pre-migration backup, restore |

## Agent playbooks

- [`agents/adding-a-job-kind.md`](./agents/adding-a-job-kind.md)
- [`agents/adding-a-page.md`](./agents/adding-a-page.md)
- [`agents/changing-the-schema.md`](./agents/changing-the-schema.md)
- [`agents/debugging.md`](./agents/debugging.md)

## Plan

- [`plan/mvp-build-order.md`](./plan/mvp-build-order.md) — the MVP task list, checked off as work progresses.
- [`plan/deferred.md`](./plan/deferred.md) — post-MVP ideas.

## Conventions

- Docs are Markdown, UTF-8.
- Each file starts with a `Last updated:` line.
- Diagrams are ASCII.
- These docs are **living**: update them in the same change that alters the behavior they describe.
