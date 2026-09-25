-- Add Gemini transcription via the hermes proxy.
--
-- One base URL configures both transports; see config::PROVIDER_GEMINI_TRANSCRIBE.
-- Store the full proxy base including the gateway path segment, e.g.
--   https://host.ts.net/google-transcribe
-- from which batch requests derive {base}/v1/transcriptions and the live
-- WebSocket derives wss://{host}/google-transcribe/v1/live.
--
-- The stored key is the *proxy* bearer token, not a Google credential: the
-- gateway holds the Gemini key and injects it upstream.
ALTER TABLE transcript_settings
ADD COLUMN geminiTranscribeBaseUrl TEXT;

ALTER TABLE transcript_settings
ADD COLUMN geminiTranscribeApiKey TEXT;
