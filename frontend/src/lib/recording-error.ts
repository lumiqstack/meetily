/**
 * `recording-error` event handling.
 *
 * The Rust side (audio/recording_state.rs `RecordingErrorPayload`) emits one
 * event per audio error. Most are recoverable and need no UI; the one with
 * `recording_stopped: true` means the backend gave up and stopped capture on
 * its own. Without a listener the UI kept showing a live recording, so this
 * runs the same stop flow as the tray: stop_recording (saves the audio,
 * clears IS_RECORDING, emits recording-stopped) and then post-processing.
 */

/** Mirrors `RecordingErrorPayload` in audio/recording_state.rs — change both together. */
export interface RecordingErrorPayload {
  message: string;
  recoverable: boolean;
  recording_stopped: boolean;
}

export interface RecordingErrorDeps {
  toastError: (title: string, options: { description: string; duration?: number }) => void;
  /** Invoke the backend stop_recording command. */
  stopRecording: () => Promise<void>;
  /** Run post-stop processing (window.handleRecordingStop(true)). */
  finishStop: () => void | Promise<void>;
}

export type RecordingErrorOutcome = 'ignored' | 'stopped' | 'stop-failed';

export async function handleRecordingError(
  payload: RecordingErrorPayload,
  deps: RecordingErrorDeps,
): Promise<RecordingErrorOutcome> {
  if (!payload.recording_stopped) {
    console.warn('[recording-error] audio error, recording continues:', payload.message);
    return 'ignored';
  }

  console.error('[recording-error] backend stopped the recording:', payload.message);
  deps.toastError('Recording stopped because of an audio error', {
    description: payload.message,
    duration: 10000,
  });

  try {
    await deps.stopRecording();
  } catch (error) {
    deps.toastError('Failed to stop recording', {
      description: error instanceof Error ? error.message : String(error),
    });
    return 'stop-failed';
  }

  await deps.finishStop();
  return 'stopped';
}
