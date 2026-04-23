# Migrations

Last updated: 2026-04-22

See [`../docs/design/14-migrations.md`](../docs/design/14-migrations.md) for the full policy.

## Rules in brief

- Migrations are **append-only**. Never edit a committed file.
- One conceptual change per file.
- Filenames: `NNNN_<snake_case_description>.sql`, sequential.
- A `VACUUM INTO` backup of the DB is taken automatically before any pending migration is applied at startup.

When changing the schema, also update `docs/design/03-data-model.md` in the same change.
