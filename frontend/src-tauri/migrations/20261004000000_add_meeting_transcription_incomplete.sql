-- Marks a meeting saved before its live transcription finished (timeout or
-- status-poll failure at Stop). Such a meeting must be re-transcribed from its
-- audio, never summarized from the partial transcript. Cleared atomically when
-- a retranscription replaces the transcript.
ALTER TABLE meetings ADD COLUMN transcription_incomplete INTEGER NOT NULL DEFAULT 0;
