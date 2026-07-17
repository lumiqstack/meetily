-- Durable journal for background batch jobs (imports, retranscriptions).
-- One row per in-flight job: inserted at job start, deleted on clean finish
-- (success, failure, cancellation). Any row still present at startup belongs
-- to a job a previous process died under; startup reconciliation flips
-- `interrupted` to 1 so the frontend can offer retry/dismiss.
CREATE TABLE IF NOT EXISTS background_jobs (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    title TEXT NOT NULL,
    source_path TEXT,
    folder_path TEXT,
    meeting_id TEXT,
    language TEXT,
    model TEXT,
    provider TEXT,
    interrupted INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL
);
