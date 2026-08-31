/**
 * Transcription provider identifiers, shared across the UI.
 *
 * These strings must match `config.rs` on the Rust side — they are the value
 * stored in `transcript_settings.provider`.
 *
 * Keep the remote check in one place: remote jobs skip the local engine guard
 * and the single-job concurrency limit, so a call site that misses a remote
 * provider silently serializes it behind on-device work.
 */

export const PROVIDER_LOCAL_WHISPER = 'localWhisper';
export const PROVIDER_PARAKEET = 'parakeet';
export const PROVIDER_OPENAI_COMPATIBLE = 'openaiCompatible';
export const PROVIDER_GEMINI_TRANSCRIBE = 'geminiTranscribe';

/** Providers that run off-device. */
export const REMOTE_TRANSCRIPTION_PROVIDERS: readonly string[] = [
  PROVIDER_OPENAI_COMPATIBLE,
  PROVIDER_GEMINI_TRANSCRIBE,
];

export function isRemoteTranscriptionProvider(provider?: string | null): boolean {
  return !!provider && REMOTE_TRANSCRIPTION_PROVIDERS.includes(provider);
}

/**
 * Providers that can transcribe an already-recorded file.
 *
 * Gemini qualifies: live recording streams over a WebSocket, but imports and
 * re-transcription always take the batch REST endpoint.
 */
export const BATCH_CAPABLE_PROVIDERS: ReadonlySet<string> = new Set([
  PROVIDER_LOCAL_WHISPER,
  'whisper', // model-list spelling of localWhisper
  PROVIDER_PARAKEET,
  PROVIDER_OPENAI_COMPATIBLE,
  PROVIDER_GEMINI_TRANSCRIBE,
]);
