-- Where an imported meeting came from, for the Obsidian note's properties.
-- One row per imported meeting; live recordings have none. `recorded_at` is
-- the Teams recording stamp (local wall-clock time, no offset) when known.
CREATE TABLE IF NOT EXISTS meeting_sources (
    meeting_id       TEXT PRIMARY KEY,
    import_source    TEXT NOT NULL,
    recording_url    TEXT,
    recording_file   TEXT,
    recorded_at      TEXT,
    duration_seconds REAL
);
