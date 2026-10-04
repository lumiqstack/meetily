import { invoke } from '@tauri-apps/api/core';
import type { TranscriptionStatus } from '@/services/transcriptService';

export interface LiveTranscriptionWaitOptions {
  getStatus: () => Promise<TranscriptionStatus>;
  /** True once a `transcription-complete` event has arrived. */
  completedByEvent: () => boolean;
  onQueue?: (chunksInQueue: number) => void;
  sleep?: (ms: number) => Promise<void>;
  maxWaitMs?: number;
  pollIntervalMs?: number;
  /** Consecutive failed status reads tolerated before giving up. */
  maxConsecutiveFailures?: number;
}

export interface LiveTranscriptionWaitResult {
  complete: boolean;
  elapsedMs: number;
  /** Why waiting ended without completion, if it did. */
  reason?: 'timeout' | 'status_errors';
}

const defaultSleep = (ms: number) => new Promise<void>(resolve => setTimeout(resolve, ms));

/**
 * Wait for the live transcription tail after Stop. A single failed status
 * read is treated as transient: reads are retried with exponential backoff
 * and the failure budget resets on any successful read.
 */
export async function waitForLiveTranscription({
  getStatus,
  completedByEvent,
  onQueue,
  sleep = defaultSleep,
  maxWaitMs = 60000,
  pollIntervalMs = 500,
  maxConsecutiveFailures = 3,
}: LiveTranscriptionWaitOptions): Promise<LiveTranscriptionWaitResult> {
  let elapsedMs = 0;
  let consecutiveFailures = 0;

  while (elapsedMs < maxWaitMs) {
    if (completedByEvent()) return { complete: true, elapsedMs };
    try {
      const status = await getStatus();
      consecutiveFailures = 0;
      if (!status.is_processing && status.chunks_in_queue === 0) {
        return { complete: true, elapsedMs };
      }
      // No activity for more than 8 seconds and an empty queue: done.
      if (status.last_activity_ms > 8000 && status.chunks_in_queue === 0) {
        return { complete: true, elapsedMs };
      }
      if (status.chunks_in_queue > 0) onQueue?.(status.chunks_in_queue);
      await sleep(pollIntervalMs);
      elapsedMs += pollIntervalMs;
    } catch (error) {
      consecutiveFailures += 1;
      console.error(
        `Error checking transcription status (${consecutiveFailures}/${maxConsecutiveFailures}):`,
        error
      );
      if (consecutiveFailures >= maxConsecutiveFailures) {
        return { complete: completedByEvent(), elapsedMs, reason: completedByEvent() ? undefined : 'status_errors' };
      }
      const backoff = pollIntervalMs * 2 ** (consecutiveFailures - 1);
      await sleep(backoff);
      elapsedMs += backoff;
    }
  }
  const complete = completedByEvent();
  return { complete, elapsedMs, reason: complete ? undefined : 'timeout' };
}

/** Where to go after saving; incomplete transcripts must not auto-summarize. */
export function postRecordingDestination(meetingId: string, transcriptionIncomplete: boolean): string {
  const source = transcriptionIncomplete ? 'recording-incomplete' : 'recording';
  return `/meeting-details?id=${meetingId}&source=${source}`;
}

async function readPipelineEnabled(): Promise<boolean> {
  const settings = await invoke<{ enabled?: boolean }>('pipeline_get_settings');
  return Boolean(settings?.enabled);
}

/**
 * Explain how an incomplete live transcript will be completed. Background
 * completion is promised only when audio was saved and the pipeline is on.
 */
export async function describeIncompleteRecovery(
  audioAvailable: boolean,
  pipelineEnabled: () => Promise<boolean> = readPipelineEnabled
): Promise<string> {
  if (!audioAvailable) {
    return 'The audio was not fully saved, so it cannot be re-transcribed automatically. Check the recording folder, or keep the partial transcript.';
  }
  let enabled = false;
  try {
    enabled = await pipelineEnabled();
  } catch (error) {
    console.warn('Could not read pipeline settings:', error);
  }
  return enabled
    ? 'It will be re-transcribed from the audio in the background; no summary is generated until then.'
    : 'Turn on background processing in Settings, or re-transcribe it from the meeting page, before summarizing.';
}
