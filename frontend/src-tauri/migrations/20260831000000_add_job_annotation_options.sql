-- Persist Gemini annotation options with the job that chose them.
--
-- A job resumed after a crash must come back with the settings the user
-- actually selected: silently retrying an authoritative pass without word
-- timestamps would replace a segmented transcript with one hour-long blob.
--
-- Existing rows default to 0/off, which matches the pre-migration behaviour.
ALTER TABLE background_jobs
ADD COLUMN diarization INTEGER NOT NULL DEFAULT 0;

ALTER TABLE background_jobs
ADD COLUMN wordTimestamps INTEGER NOT NULL DEFAULT 0;
