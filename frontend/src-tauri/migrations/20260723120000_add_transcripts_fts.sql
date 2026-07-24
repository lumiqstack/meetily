-- Full-text search over transcript segments (FTS5, external-content).
--
-- The index mirrors transcripts.transcript via row-level triggers, so every
-- write path — recording save, import, and retranscription's delete-all +
-- re-insert — keeps it in sync with no application-code involvement.
CREATE VIRTUAL TABLE transcripts_fts USING fts5(
    transcript,
    content='transcripts',
    content_rowid='rowid',
    tokenize='unicode61 remove_diacritics 2'
);

CREATE TRIGGER transcripts_fts_ai AFTER INSERT ON transcripts BEGIN
    INSERT INTO transcripts_fts(rowid, transcript)
    VALUES (new.rowid, new.transcript);
END;

CREATE TRIGGER transcripts_fts_ad AFTER DELETE ON transcripts BEGIN
    INSERT INTO transcripts_fts(transcripts_fts, rowid, transcript)
    VALUES ('delete', old.rowid, old.transcript);
END;

CREATE TRIGGER transcripts_fts_au AFTER UPDATE OF transcript ON transcripts BEGIN
    INSERT INTO transcripts_fts(transcripts_fts, rowid, transcript)
    VALUES ('delete', old.rowid, old.transcript);
    INSERT INTO transcripts_fts(rowid, transcript)
    VALUES (new.rowid, new.transcript);
END;

-- Backfill: index every transcript that predates this migration.
INSERT INTO transcripts_fts(rowid, transcript)
SELECT rowid, transcript FROM transcripts;
