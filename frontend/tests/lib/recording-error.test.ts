import { describe, expect, test } from 'bun:test';
import { handleRecordingError, type RecordingErrorDeps } from '../../src/lib/recording-error';

function fakeDeps(overrides: Partial<RecordingErrorDeps> = {}) {
  const calls: string[] = [];
  const toasts: string[] = [];
  const deps: RecordingErrorDeps = {
    toastError: (title) => { toasts.push(title); },
    stopRecording: async () => { calls.push('stop'); },
    finishStop: () => { calls.push('finish'); },
    ...overrides,
  };
  return { deps, calls, toasts };
}

describe('handleRecordingError', () => {
  test('recoverable error that did not stop recording is ignored', async () => {
    const { deps, calls, toasts } = fakeDeps();
    const outcome = await handleRecordingError(
      { message: 'Audio device disconnected', recoverable: true, recording_stopped: false },
      deps,
    );
    expect(outcome).toBe('ignored');
    expect(calls).toEqual([]);
    expect(toasts).toEqual([]);
  });

  test('stop event toasts, stops the backend, then runs post-processing', async () => {
    const { deps, calls, toasts } = fakeDeps();
    const outcome = await handleRecordingError(
      { message: 'Audio device disconnected', recoverable: true, recording_stopped: true },
      deps,
    );
    expect(outcome).toBe('stopped');
    expect(calls).toEqual(['stop', 'finish']);
    expect(toasts).toEqual(['Recording stopped because of an audio error']);
  });

  test('failed backend stop skips post-processing and reports the failure', async () => {
    const { deps, calls, toasts } = fakeDeps({
      stopRecording: async () => { throw new Error('boom'); },
    });
    const outcome = await handleRecordingError(
      { message: 'Permission denied', recoverable: false, recording_stopped: true },
      deps,
    );
    expect(outcome).toBe('stop-failed');
    expect(calls).toEqual([]);
    expect(toasts).toEqual(['Recording stopped because of an audio error', 'Failed to stop recording']);
  });
});
