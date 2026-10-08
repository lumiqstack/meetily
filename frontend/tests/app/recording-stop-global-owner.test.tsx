import { afterAll, beforeAll, describe, expect, mock, test } from 'bun:test';
import type React from 'react';
import { act, create } from 'react-test-renderer';

// app/page.tsx and contexts/RecordingPostProcessingProvider.tsx both call
// useRecordingStop. window.handleRecordingStop is a single global, so only the
// app-level provider may own it: unmounting the page must not remove the handler
// that TeamsMeetingDetectionProvider and Rust-triggered stops still need.
const originals = {
  recordingState: null as unknown as Record<string, unknown>,
  transcriptContext: { ...await import('../../src/contexts/TranscriptContext') },
  navigation: { ...await import('next/navigation') },
  event: { ...await import('@tauri-apps/api/event') },
  core: { ...await import('@tauri-apps/api/core') },
  storage: { ...await import('../../src/services/storageService') },
  transcript: { ...await import('../../src/services/transcriptService') },
  analytics: { ...await import('../../src/lib/analytics') },
  prefs: { ...await import('../../src/lib/summary-language-preferences') },
  sonner: { ...await import('sonner') },
};
afterAll(() => {
  if (originals.recordingState) mock.module('../../src/contexts/RecordingStateContext', () => originals.recordingState);
  mock.module('../../src/contexts/TranscriptContext', () => originals.transcriptContext);
  mock.module('next/navigation', () => originals.navigation);
  mock.module('@tauri-apps/api/event', () => originals.event);
  mock.module('@tauri-apps/api/core', () => originals.core);
  mock.module('../../src/services/storageService', () => originals.storage);
  mock.module('../../src/services/transcriptService', () => originals.transcript);
  mock.module('../../src/lib/analytics', () => originals.analytics);
  mock.module('../../src/lib/summary-language-preferences', () => originals.prefs);
  mock.module('sonner', () => originals.sonner);
  delete (globalThis as any).window;
});

mock.module('../../src/contexts/TranscriptContext', () => ({
  ...originals.transcriptContext,
  useTranscripts: () => ({
    transcriptsRef: { current: [] }, flushBuffer() {}, clearTranscripts() {},
    meetingTitle: 'Untitled', markMeetingAsSaved() {},
  }),
}));
mock.module('next/navigation', () => ({ usePathname: () => '/', useRouter: () => ({ push() {} }) }));
mock.module('@tauri-apps/api/core', () => ({ invoke: async () => [] }));
mock.module('@tauri-apps/api/event', () => ({ ...originals.event, listen: async () => () => {} }));
mock.module('../../src/services/storageService', () => ({ storageService: {} }));
mock.module('../../src/services/transcriptService', () => ({ transcriptService: {} }));
mock.module('../../src/lib/analytics', () => ({ default: { trackBackendConnection() {}, trackButtonClick() {}, trackPageView() {} } }));
mock.module('../../src/lib/summary-language-preferences', () => ({
  applyPinnedSummaryLanguageToMeeting: async () => {},
  detectAndCacheSummaryLanguage: async () => ({ language: 'en' }),
}));
mock.module('sonner', () => ({ toast: { success() {}, error() {}, info() {}, warning() {} } }));

// The hook is imported at run time, after this file installs a complete
// RecordingStateContext mock, so it never links to a partial mock from another file.
let SidebarProvider: (props: { children: React.ReactNode }) => React.ReactNode;
let RecordingPostProcessingProvider: (props: { children: React.ReactNode }) => React.ReactNode;
let useRecordingStop: (setIsRecording: (v: boolean) => void, setIsRecordingDisabled: (v: boolean) => void) => unknown;
beforeAll(async () => {
  originals.recordingState = { ...await import('../../src/contexts/RecordingStateContext') };
  mock.module('../../src/contexts/RecordingStateContext', () => ({
    RecordingStatus: { IDLE: 'idle', PROCESSING_TRANSCRIPTS: 'processing', STOPPING: 'stopping', SAVING: 'saving', COMPLETED: 'completed', ERROR: 'error' },
    useRecordingState: () => ({
      status: 'idle', setStatus() {}, isStopping: false, isProcessing: false, isSaving: false,
    }),
  }));
  ({ useRecordingStop } = await import('../../src/hooks/useRecordingStop'));
  ({ RecordingPostProcessingProvider } = await import('../../src/contexts/RecordingPostProcessingProvider'));
  ({ SidebarProvider } = await import('../../src/components/Sidebar/SidebarProvider'));
});

// Mounted in app/page.tsx: calls the hook but does not own the window global.
function PageConsumer() {
  useRecordingStop(() => {}, () => {});
  return null;
}
const wrap = (node: React.ReactNode) => <SidebarProvider>{node}</SidebarProvider>;

describe('window.handleRecordingStop ownership', () => {
  test('unmounting the page consumer keeps window.handleRecordingStop for RecordingPostProcessingProvider', async () => {
    (globalThis as any).window ??= globalThis;
    delete (window as any).handleRecordingStop;

    let provider!: ReturnType<typeof create>;
    let page!: ReturnType<typeof create>;
    await act(async () => {
      provider = create(wrap(<RecordingPostProcessingProvider><></></RecordingPostProcessingProvider>));
      page = create(wrap(<PageConsumer />));
    });
    expect(typeof (window as any).handleRecordingStop).toBe('function');

    await act(async () => {
      page.unmount();
    });

    expect(typeof (window as any).handleRecordingStop).toBe('function');

    await act(async () => {
      provider.unmount();
    });
    expect((window as any).handleRecordingStop).toBeUndefined();
  });
});
