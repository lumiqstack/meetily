import { afterAll, describe, expect, mock, test } from 'bun:test';
import { act, create, type ReactTestRenderer } from 'react-test-renderer';

// H8-F5: the transcription worker emits `transcript-chunk-loss-detected` with
// an object payload (src-tauri/src/audio/transcription/worker.rs). The UI must
// listen for that event and read that shape, or the chunk-loss modal never shows.
const originalEvent = { ...await import('@tauri-apps/api/event') };
const originalSonner = { ...await import('sonner') };
afterAll(() => {
  mock.module('@tauri-apps/api/event', () => originalEvent);
  mock.module('sonner', () => originalSonner);
});

const handlers = new Map<string, (event: { payload: unknown }) => void>();
mock.module('@tauri-apps/api/event', () => ({
  ...originalEvent,
  listen: async (name: string, handler: (event: { payload: unknown }) => void) => {
    handlers.set(name, handler);
    return () => handlers.delete(name);
  },
}));
mock.module('sonner', () => ({ toast: { success: () => {}, error: () => {}, info: () => {}, warning: () => {} } }));
const { useModalState } = await import('../../src/hooks/useModalState');
const { recordingService } = await import('../../src/services/recordingService');

// Exactly what worker.rs serialises with serde_json::json!.
const rustPayload = {
  chunks_queued: 12,
  chunks_completed: 9,
  chunks_lost: 3,
  message: 'Some transcript chunks may have been lost during shutdown',
};

describe('transcript chunk loss', () => {
  test('the modal opens with the loss details when the Rust event fires', async () => {
    let current: ReturnType<typeof useModalState> | undefined;
    function Probe() {
      current = useModalState();
      return null;
    }
    let renderer: ReactTestRenderer | undefined;
    await act(async () => {
      renderer = create(<Probe />);
    });

    const handler = handlers.get('transcript-chunk-loss-detected');
    expect(handler).toBeDefined();
    await act(async () => handler!({ payload: rustPayload }));

    expect(current!.modals.chunkDropWarning).toBe(true);
    expect(current!.messages.chunkDropWarning).toBe(
      'Some transcript chunks may have been lost during shutdown (3 of 12 chunks lost).',
    );

    await act(async () => renderer!.unmount());
    expect(handlers.has('transcript-chunk-loss-detected')).toBe(false);
  });

  test('recordingService forwards the Rust payload unchanged', async () => {
    const received: unknown[] = [];
    const unlisten = await recordingService.onTranscriptChunkLossDetected((payload) => received.push(payload));
    handlers.get('transcript-chunk-loss-detected')!({ payload: rustPayload });
    expect(received).toEqual([rustPayload]);
    unlisten();
  });
});
