import { describe, expect, test } from 'bun:test';
import { describeIncompleteRecovery, postRecordingDestination, waitForLiveTranscription } from '../../src/lib/live-transcription-wait';
import type { TranscriptionStatus } from '../../src/services/transcriptService';

const busy: TranscriptionStatus = { chunks_in_queue: 2, is_processing: true, last_activity_ms: 100 };
const idle: TranscriptionStatus = { chunks_in_queue: 0, is_processing: false, last_activity_ms: 100 };
const noSleep = async () => {};

function sequence(steps: Array<TranscriptionStatus | Error>) {
  let i = 0;
  return async () => {
    const step = steps[Math.min(i++, steps.length - 1)];
    if (step instanceof Error) throw step;
    return step;
  };
}

describe('waitForLiveTranscription', () => {
  test('normal completion', async () => {
    const result = await waitForLiveTranscription({
      getStatus: sequence([busy, busy, idle]), completedByEvent: () => false, sleep: noSleep,
    });
    expect(result).toEqual({ complete: true, elapsedMs: 1000 });
  });

  test('timeout leaves the transcript incomplete', async () => {
    const result = await waitForLiveTranscription({
      getStatus: sequence([busy]), completedByEvent: () => false, sleep: noSleep, maxWaitMs: 2000,
    });
    expect(result.complete).toBe(false);
    expect(result.reason).toBe('timeout');
  });

  test('a transient status error is retried and the budget resets on success', async () => {
    const failure = new Error('ipc');
    const result = await waitForLiveTranscription({
      getStatus: sequence([failure, failure, busy, failure, failure, idle]),
      completedByEvent: () => false,
      sleep: noSleep,
    });
    expect(result.complete).toBe(true);
  });

  test('persistent status errors end the wait as incomplete', async () => {
    let calls = 0;
    const result = await waitForLiveTranscription({
      getStatus: async () => { calls += 1; throw new Error('ipc'); },
      completedByEvent: () => false,
      sleep: noSleep,
    });
    expect(calls).toBe(3);
    expect(result).toMatchObject({ complete: false, reason: 'status_errors' });
  });

  test('the transcription-complete event wins even after status errors', async () => {
    let done = false;
    const result = await waitForLiveTranscription({
      getStatus: async () => { done = true; throw new Error('ipc'); },
      completedByEvent: () => done,
      sleep: noSleep,
    });
    expect(result.complete).toBe(true);
  });

  test('incomplete recordings never navigate with the auto-summary source', () => {
    expect(postRecordingDestination('m1', false)).toBe('/meeting-details?id=m1&source=recording');
    expect(postRecordingDestination('m1', true)).toBe('/meeting-details?id=m1&source=recording-incomplete');
  });

  test('recovery message only promises background completion when it can happen', async () => {
    expect(await describeIncompleteRecovery(true, async () => true)).toContain('re-transcribed from the audio in the background');
    expect(await describeIncompleteRecovery(true, async () => false)).toContain('Turn on background processing');
    expect(await describeIncompleteRecovery(true, async () => { throw new Error('ipc'); })).toContain('Turn on background processing');
    const noAudio = await describeIncompleteRecovery(false, async () => true);
    expect(noAudio).toContain('cannot be re-transcribed automatically');
    expect(noAudio).not.toContain('in the background');
  });
});
