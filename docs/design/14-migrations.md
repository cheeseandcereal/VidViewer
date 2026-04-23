# 14 — Migrations and backups

Last updated: 2026-04-22

Schema migrations live in `migrations/NNNN_description.sql` at the repo root and are applied
via `sqlx::migrate!` at startup.

## Rules

- **Append-only.** Once a migration is committed, never edit it. To correct a mistake, add a
  new migration that compensates.
- **One conceptual change per file.** Keeps review and rollback comprehensible.
- **Numbered strictly ascending.** Use `0001_initial.sql`, `0002_...sql`, etc.

## Pre-migration backup

Before any pending migration runs, a backup of the current database is written to
`~/.local/share/vidviewer/backups/` (overridable via `backup_dir` in config).

### When

- Only when `sqlx::migrate!` detects at least one pending migration.
- Fresh installs (no existing DB file) skip backup and log a note.

### How

`VACUUM INTO '<backup_path>'` against the live pool. This produces a single consistent
`.db` file on any SQLite 3.27+, including when WAL is in use.

### Filename

```
vidviewer-<UTC-timestamp>-pre-migration-v<N>.db
```

Where `<N>` is the schema version **before** migrations ran (i.e. the version the backup
is safe to restore against).

### Retention

All backups are kept; nothing is auto-pruned. Disk usage is the user's responsibility.
A backup of a moderate personal library is typically < 50 MB; heavy libraries a few hundred MB.

### Failure handling

- If the backup fails for any reason (disk full, permission denied, SQLite error), the app
  logs a clear error and exits non-zero. **Migrations do not run.** The user must resolve
  the backup issue first.
- If a migration fails *after* a successful backup, the app exits with the backup path in
  the error message so the user can restore manually.

## Manual restore

1. Stop the server.
2. ```sh
   mv ~/.local/share/vidviewer/vidviewer.db ~/.local/share/vidviewer/vidviewer.db.broken
   cp <backup_path> ~/.local/share/vidviewer/vidviewer.db
   ```
3. Start the server using a binary compatible with the schema version in the backup filename.

An automated `vidviewer restore-backup` subcommand is deferred.
