import { afterAll, beforeAll, describe, expect, mock, test } from 'bun:test';
import { useSyncExternalStore } from 'react';
import { act, create, type ReactTestRenderer } from 'react-test-renderer';

// After a successful save, useRecordingStop schedules a 2 s auto-navigation that
// also clears transcripts and resets status to IDLE. Start is re-enabled
// immediately, so a recording started inside that window must not be torn down.
const originals = {
  recordingState: null as unknown as Record<string, unknown>,
  transcriptContext: { ...await import('../../src/contexts/TranscriptContext') },
  sidebar: { ...await import('../../src/components/Sidebar/SidebarProvider') },
  navigation: { ...await import('next/navigation') },
  event: { ...await import('@tauri-apps/api/event') },
  storage: { ...await import('../../src/services/storageService') },
  transcript: { ...await import('../../src/services/transcriptService') },
  analytics: { ...await import('../../src/lib/analytics') },
  prefs: { ...await import('../../src/lib/summary-language-preferences') },
  sonner: { ...await import('sonner') },
};
const hadWindow = 'window' in globalThis;
const hadSessionStorage = 'sessionStorage' in globalThis;
afterAll(() => {
  if (originals.recordingState) mock.module('../../src/contexts/RecordingStateContext', () => originals.recordingState);
  mock.module('../../src/contexts/TranscriptContext', () => originals.transcriptContext);
  mock.module('../../src/components/Sidebar/SidebarProvider', () => originals.sidebar);
  mock.module('next/navigation', () => originals.navigation);
  mock.module('@tauri-apps/api/event', () => originals.event);
  mock.module('../../src/services/storageService', () => originals.storage);
  mock.module('../../src/services/transcriptService', () => originals.transcript);
  mock.module('../../src/lib/analytics', () => originals.analytics);
  mock.module('../../src/lib/summary-language-preferences', () => originals.prefs);
  mock.module('sonner', () => originals.sonner);
  if (!hadWindow) delete (globalThis as any).window;
  if (!hadSessionStorage) delete (globalThis as any).sessionStorage;
});

let liveStatus = 'idle';
const statusListeners = new Set<() => void>();
const setStatus = (status: string) => {
  liveStatus = status;
  statusListeners.forEach(fn => fn());
};
const subscribeStatus = (fn: () => void) => {
  statusListeners.add(fn);
  return () => { statusListeners.delete(fn); };
};

const push = mock((_url: string) => {});
const clearTranscripts = mock(() => {});

mock.module('../../src/contexts/TranscriptContext', () => ({
  ...originals.transcriptContext,
  useTranscripts: () => ({
    transcriptsRef: { current: [{ text: 'hello', audio_start_time: 0 }] },
    flushBuffer() {},
    clearTranscripts,
    meetingTitle: 'Untitled',
    markMeetingAsSaved: async () => {},
  }),
}));
mock.module('../../src/components/Sidebar/SidebarProvider', () => ({
  ...originals.sidebar,
  useSidebar: () => ({
    refetchMeetings: async () => {},
    setCurrentMeeting() {},
    setMeetings() {},
    meetings: [],
    setIsMeetingActive() {},
  }),
}));
mock.module('next/navigation', () => ({ usePathname: () => '/', useRouter: () => ({ push }) }));
mock.module('@tauri-apps/api/event', () => ({ ...originals.event, listen: async () => () => {} }));
mock.module('../../src/services/storageService', () => ({
  storageService: {
    saveMeeting: async () => ({ meeting_id: 'meeting-1' }),
    getMeeting: async () => ({ title: 'Saved meeting' }),
  },
}));
mock.module('../../src/services/transcriptService', () => ({ transcriptService: {} }));
mock.module('../../src/lib/analytics', () => ({ default: { trackButtonClick() {}, trackPageView() {} } }));
mock.module('../../src/lib/summary-language-preferences', () => ({
  applyPinnedSummaryLanguageToMeeting: async () => true,
  detectAndCacheSummaryLanguage: async () => ({ language: 'en' }),
}));
mock.module('sonner', () => ({ toast: { success() {}, error() {}, info() {}, warning() {} } }));

let useRecordingStop: typeof import('../../src/hooks/useRecordingStop').useRecordingStop;
beforeAll(async () => {
  originals.recordingState = { ...await import('../../src/contexts/RecordingStateContext') };
  mock.module('../../src/contexts/RecordingStateContext', () => ({
    RecordingStatus: {
      IDLE: 'idle', STARTING: 'starting', RECORDING: 'recording', STOPPING: 'stopping',
      PROCESSING_TRANSCRIPTS: 'processing', SAVING: 'saving', COMPLETED: 'completed', ERROR: 'error',
    },
    useRecordingState: () => {
      const status = useSyncExternalStore(subscribeStatus, () => liveStatus);
      return { status, setStatus, isStopping: false, isProcessing: false, isSaving: false };
    },
  }));
  ({ useRecordingStop } = await import('../../src/hooks/useRecordingStop'));
});

// Runs a successful stop with the 2 s navigation timer held; the stop flow's
// shorter waits run immediately.
async function completeStop() {
  (globalThis as any).window ??= globalThis;
  const store = new Map<string, string>([
    ['last_recording_realtime_transcription_enabled', 'false'],
    ['last_recording_audio_save_enabled', 'true'],
  ]);
  (globalThis as any).sessionStorage = {
    getItem: (k: string) => store.get(k) ?? null,
    setItem: (k: string, v: string) => { store.set(k, String(v)); },
    removeItem: (k: string) => { store.delete(k); },
  };

  const realSetTimeout = globalThis.setTimeout;
  const deferred: Array<() => void> = [];
  (globalThis as any).setTimeout = (fn: () => void, ms?: number) => {
    if (ms === 2000) {
      deferred.push(fn);
      return 0;
    }
    return realSetTimeout(fn, 0);
  };

  let handleRecordingStop!: (callApi: boolean) => Promise<void>;
  function Probe() {
    ({ handleRecordingStop } = useRecordingStop(() => {}, () => {}));
    return null;
  }

  let renderer: ReactTestRenderer | undefined;
  const cleanup = async () => {
    (globalThis as any).setTimeout = realSetTimeout;
    await act(async () => renderer?.unmount());
  };
  try {
    await act(async () => {
      renderer = create(<Probe />);
    });
    await act(async () => {
      await handleRecordingStop(true);
    });
    expect(liveStatus).toBe('completed');
    expect(deferred).toHaveLength(1);
  } catch (error) {
    await cleanup();
    throw error;
  }

  return {
    fireTimer: () => act(async () => { deferred.forEach(fn => fn()); }),
    cleanup,
  };
}

describe('post-save auto-navigation', () => {
  test('a recording started within the 2 s window is not navigated away, cleared, or reset to IDLE', async () => {
    push.mockClear();
    const run = await completeStop();
    try {
      // A new recording starts before the timer fires (button, tray, shortcut, Teams).
      await act(async () => {
        setStatus('starting');
        setStatus('recording');
      });
      clearTranscripts.mockClear();

      await run.fireTimer();

      expect(push).not.toHaveBeenCalled();
      expect(clearTranscripts).not.toHaveBeenCalled();
      expect(liveStatus).toBe('recording');
    } finally {
      await run.cleanup();
    }
  });

  test('with no new recording, the timer navigates to the saved meeting and resets to IDLE', async () => {
    push.mockClear();
    const run = await completeStop();
    try {
      clearTranscripts.mockClear();
      await run.fireTimer();

      expect(push.mock.calls).toEqual([['/meeting-details?id=meeting-1&source=recording']]);
      expect(clearTranscripts).toHaveBeenCalledTimes(1);
      expect(liveStatus).toBe('idle');
    } finally {
      await run.cleanup();
    }
  });
});
