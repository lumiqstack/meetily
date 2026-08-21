-- Indexes for the per-meeting lookups that dominate startup and the pipeline tick.
--
-- transcripts.meeting_id carries a FOREIGN KEY, but SQLite does not index
-- foreign keys, so every `WHERE meeting_id = ?` was a full scan of transcripts:
-- get_pending_meetings, get_meeting, both halves of
-- get_meeting_transcripts_paginated, delete_meeting's cascade, and
-- TranscriptRepository's speaker list and delete-all paths.
--
-- meetings(created_at DESC) serves the ORDER BY shared by get_meetings and
-- get_pending_meetings.
CREATE INDEX IF NOT EXISTS idx_transcripts_meeting_id ON transcripts(meeting_id);
CREATE INDEX IF NOT EXISTS idx_meetings_created_at ON meetings(created_at DESC);
