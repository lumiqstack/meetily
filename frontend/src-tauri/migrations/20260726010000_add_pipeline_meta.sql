-- Advisory retry bookkeeping for the automatic meeting pipeline.
--
-- The pipeline's work list is DERIVED on every tick from the same query the
-- pending-work panel uses (meetings with a folder but no transcripts, or
-- transcripts without a completed summary), so it is crash-safe by
-- construction and needs no stored stage. What the derived query cannot
-- express is "this meeting already failed N times, back off / stop trying" —
-- that is what this table records.
--
-- Purely advisory: deleting a row only resets retry counters, and losing sync
-- with reality can never orphan or duplicate work.
CREATE TABLE IF NOT EXISTS pipeline_meta (
    meeting_id    TEXT PRIMARY KEY,
    attempts      INTEGER NOT NULL DEFAULT 0,
    last_stage    TEXT,
    last_error    TEXT,
    next_retry_at TEXT,
    suppressed    INTEGER NOT NULL DEFAULT 0,
    updated_at    TEXT NOT NULL
);
