-- Consecutive summary timeouts for a meeting. A timeout increments it, any
-- other outcome and a success reset it. At the limit the meeting is suppressed
-- until the user retries it by hand, which deletes the row.
ALTER TABLE pipeline_meta ADD COLUMN consecutive_timeouts INTEGER NOT NULL DEFAULT 0;
