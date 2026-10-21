import { afterAll, describe, expect, mock, test } from 'bun:test';
import { act, create, type ReactTestRenderer } from 'react-test-renderer';

// TranscriptProvider's transcript-update listener must stay attached across a
// meeting-id change. Tauri does not buffer events, so any re-subscription gap
// (or a cleared flush timer) loses transcript segments from the UI and the save.
const originals = {
  event: { ...await import('@tauri-apps/api/event') },
  core: { ...await import('@tauri-apps/api/core') },
  indexedDB: { ...await import('../../src/services/indexedDBService') },
  recordingState: { ...await import('../../src/contexts/RecordingStateContext') },
};
const hadSessionStorage = 'sessionStorage' in globalThis;
afterAll(() => {
  mock.module('@tauri-apps/api/event', () => originals.event);
  mock.module('@tauri-apps/api/core', () => originals.core);
  mock.module('../../src/services/indexedDBService', () => originals.indexedDB);
  mock.module('../../src/contexts/RecordingStateContext', () => originals.recordingState);
  if (!hadSessionStorage) delete (globalThis as any).sessionStorage;
});

if (!hadSessionStorage) {
  const store = new Map<string, string>();
  (globalThis as any).sessionStorage = {
    getItem: (k: string) => store.get(k) ?? null,
    setItem: (k: string, v: string) => { store.set(k, v); },
    removeItem: (k: string) => { store.delete(k); },
  };
}

type Handler = (event: { payload: unknown }) => void;
const handlers = new Map<string, Set<Handler>>();
// While set, new listen() calls stay pending until released, like a slow IPC round trip.
let heldListens: (() => void)[] | null = null;

mock.module('@tauri-apps/api/event', () => ({
  ...originals.event,
  listen: (name: string, handler: Handler) => {
    const attach = () => {
      if (!handlers.has(name)) handlers.set(name, new Set());
      handlers.get(name)!.add(handler);
      return () => { handlers.get(name)?.delete(handler); };
    };
    if (heldListens) {
      const held = heldListens;
      return new Promise<() => void>(resolve => held.push(() => resolve(attach())));
    }
    return Promise.resolve(attach());
  },
}));
mock.module('@tauri-apps/api/core', () => ({ ...originals.core, invoke: async () => null }));
mock.module('../../src/services/indexedDBService', () => ({
  indexedDBService: {
    init: async () => {},
    saveMeetingMetadata: async () => {},
    getMeetingMetadata: async () => null,
    saveTranscript: async () => {},
    markMeetingSaved: async () => {},
  },
}));
mock.module('../../src/contexts/RecordingStateContext', () => ({
  ...originals.recordingState,
  useRecordingState: () => ({ isRecording: false }),
}));

const { TranscriptProvider, useTranscripts } = await import('../../src/contexts/TranscriptContext');

const emit = (name: string, payload: unknown) => {
  for (const handler of [...(handlers.get(name) ?? [])]) handler({ payload });
};
const update = (sequence_id: number, text: string) => ({
  text,
  timestamp: '00:00',
  source: 'mic',
  sequence_id,
  chunk_start_time: sequence_id,
  is_partial: false,
  confidence: 1,
  audio_start_time: sequence_id,
  audio_end_time: sequence_id + 1,
  duration: 1,
});
const settle = (ms = 30) => act(() => new Promise<void>(resolve => setTimeout(resolve, ms)));

async function mountProvider() {
  handlers.clear();
  heldListens = null;
  let ctx: ReturnType<typeof useTranscripts> | undefined;
  function Probe() {
    ctx = useTranscripts();
    return null;
  }
  let renderer: ReactTestRenderer | undefined;
  await act(async () => {
    renderer = create(<TranscriptProvider><Probe /></TranscriptProvider>);
  });
  await settle();
  return { ctx: () => ctx!, renderer: renderer! };
}

describe('TranscriptProvider transcript-update listener', () => {
  test('a segment emitted right after recording-started reaches the transcript list', async () => {
    const { ctx, renderer } = await mountProvider();

    heldListens = [];
    await act(async () => emit('recording-started', null));
    await settle();
    expect(ctx().currentMeetingId).not.toBeNull();

    await act(async () => emit('transcript-update', update(1, 'first words')));
    const held = heldListens;
    heldListens = null;
    await act(async () => held.forEach(release => release()));
    await settle();

    expect(ctx().transcripts.map(t => t.text)).toEqual(['first words']);
    await act(async () => renderer.unmount());
  });

  test('a buffered segment is not dropped when the meeting id is cleared', async () => {
    const { ctx, renderer } = await mountProvider();

    await act(async () => emit('recording-started', null));
    await settle();

    await act(async () => {
      emit('transcript-update', update(2, 'last words'));
      await ctx().markMeetingAsSaved();
    });
    await settle();

    expect(ctx().currentMeetingId).toBeNull();
    expect(ctx().transcripts.map(t => t.text)).toEqual(['last words']);
    await act(async () => renderer.unmount());
  });
});
