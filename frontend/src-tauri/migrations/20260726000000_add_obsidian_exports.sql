-- Tracks the exact filename each meeting was exported to in the Obsidian
-- vault. The user's own vault notes link to these filenames, so once a
-- meeting has been exported its filename must never change across
-- re-exports (re-summarization) even if the title or filename template
-- changes. One row per meeting; upserted after every successful write.
CREATE TABLE IF NOT EXISTS obsidian_exports (
    meeting_id  TEXT PRIMARY KEY,
    filename    TEXT NOT NULL,
    exported_at TEXT NOT NULL
);
