-- 0002_jobs_unique_outstanding.sql
-- Add a partial unique index that prevents two pending/running jobs from existing for
-- the same (kind, video_id). This is a DB-level backstop for the idempotent
-- `enqueue_on` in src/jobs/mod.rs.
--
-- Append-only: never edit this file after commit.

-- Before creating the index, clean up any duplicates left behind by the pre-index
-- era. Keep the lowest-id outstanding row per (kind, video_id); delete the rest.
DELETE FROM jobs
WHERE status IN ('pending', 'running')
  AND id NOT IN (
      SELECT MIN(id)
      FROM jobs
      WHERE status IN ('pending', 'running')
      GROUP BY kind, video_id
  );

CREATE UNIQUE INDEX idx_jobs_outstanding_unique
    ON jobs(kind, video_id)
    WHERE status IN ('pending', 'running');
