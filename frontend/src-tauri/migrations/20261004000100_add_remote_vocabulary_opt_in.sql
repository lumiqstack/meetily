-- Explicit opt-in to send the user's vocabulary list to the configured remote
-- (OpenAI-compatible) transcription server as the `prompt` field. Off for
-- existing and new users: local vocabulary never leaves the machine unless
-- the user turns this on.
ALTER TABLE transcript_settings ADD COLUMN remoteVocabularyEnabled INTEGER NOT NULL DEFAULT 0;
