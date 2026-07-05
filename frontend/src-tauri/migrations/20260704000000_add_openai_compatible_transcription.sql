-- Add remote OpenAI-compatible transcription provider settings.
-- Base URL points at any server exposing POST {base}/v1/audio/transcriptions
-- (e.g. oMLX, LiteLLM, vLLM, or the official OpenAI API).
ALTER TABLE transcript_settings
ADD COLUMN openaiCompatibleBaseUrl TEXT;

ALTER TABLE transcript_settings
ADD COLUMN openaiCompatibleApiKey TEXT;
