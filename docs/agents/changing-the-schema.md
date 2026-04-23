# Agent playbook — Changing the schema

Last updated: 2026-04-22

1. **Add a new migration.** Never edit committed ones.
   - Filename: `migrations/NNNN_<snake_case_description>.sql`, where `NNNN` is the next
     sequential four-digit number.
   - One conceptual change per migration.
2. **Backups are automatic.** The app creates a `VACUUM INTO` backup of the current DB
   before running any pending migration at startup. You don't need to add backup logic.
3. **Run locally.**
   ```
   just run    # this will apply the migration and back up the old DB
   ```
4. **Refresh sqlx offline metadata.**
   ```
   just prepare-sqlx
   ```
   Commit the updated `.sqlx/` files.
5. **Update `docs/design/03-data-model.md`** to reflect the new schema.
6. **Check everything.**
   ```
   just fmt && just lint && just check && just test
   ```

## Forbidden

- Editing any migration file that has been committed.
- Dropping tables or columns that contain user data without first preserving it in a follow-up
  migration.
- Skipping the data-model doc update.
