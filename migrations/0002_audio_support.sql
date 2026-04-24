-- 0002_audio_support.sql
--
-- Adds audio-file support to the videos table. `is_audio_only = 1` means the
-- row is an audio file (either by extension-neutral sniff + ffprobe saying
-- "no real video streams", or a video container carrying only audio + an
-- optional attached_pic). `attached_pic_stream_index`, when non-NULL, is the
-- zero-based index of a still-image stream that can be used as a thumbnail
-- (typically embedded cover art).
--
-- Existing rows default to `is_audio_only = 0` and `attached_pic_stream_index
-- = NULL`. They will be reclassified the next time their probe job runs
-- (after a file change or a manual rescan). No forced reprobe on migration.

ALTER TABLE videos ADD COLUMN is_audio_only INTEGER NOT NULL DEFAULT 0
    CHECK (is_audio_only IN (0, 1));

ALTER TABLE videos ADD COLUMN attached_pic_stream_index INTEGER;
