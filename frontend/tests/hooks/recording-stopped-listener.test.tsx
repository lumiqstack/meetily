import { afterAll, beforeAll, describe, expect, mock, test } from 'bun:test';
import { act, create, type ReactTestRenderer } from 'react-test-renderer';

// useRecordingStop's `recording-stopped` listener must not leak when the effect
// cleans up (unmount, or a new router identity) while listen() is still pending.
// A leaked listener shows the "audio was not fully saved" warning once per leak.
const originals = {
  recordingState: null as unknown as Record<string, unknown>,
  transcriptContext: { ...await import('../../src/contexts/TranscriptContext') },
  sidebar: { ...await import('../../src/components/Sidebar/SidebarProvider') },
  navigation: { ...await import('next/navigation') },
  event: { ...await import('@tauri-apps/api/event') },
  sonner: { ...await import('sonner') },
};
const hadSessionStorage = 'sessionStorage' in globalThis;
afterAll(() => {
  if (originals.recordingState) mock.module('../../src/contexts/RecordingStateContext', () => originals.recordingState);
  mock.module('../../src/contexts/TranscriptContext', () => originals.transcriptContext);
  mock.module('../../src/components/Sidebar/SidebarProvider', () => originals.sidebar);
  mock.module('next/navigation', () => originals.navigation);
  mock.module('@tauri-apps/api/event', () => originals.event);
  mock.module('sonner', () => originals.sonner);
  if (!hadSessionStorage) delete (globalThis as any).sessionStorage;
});
(globalThis as any).sessionStorage ??= { getItem: () => null, setItem() {}, removeItem() {} };

type Registration = {
  name: string;
  handler: (event: { payload: unknown }) => void;
  unlistened: boolean;
  resolve: () => void;
};
const registrations: Registration[] = [];
const pending: Registration[] = [];
mock.module('@tauri-apps/api/event', () => ({
  ...originals.event,
  listen: (name: string, handler: Registration['handler']) =>
    new Promise<() => void>(resolveListen => {
      const registration: Registration = {
        name,
        handler,
        unlistened: false,
        resolve: () => resolveListen(() => { registration.unlistened = true; }),
      };
      registrations.push(registration);
      pending.push(registration);
    }),
}));
mock.module('../../src/contexts/TranscriptContext', () => ({
  ...originals.transcriptContext,
  useTranscripts: () => ({
    transcriptsRef: { current: [] },
    flushBuffer() {},
    clearTranscripts() {},
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
// A fresh router object per render re-runs the effect that depends on it.
mock.module('next/navigation', () => ({ usePathname: () => '/', useRouter: () => ({ push() {} }) }));
const warning = mock((_message: string, _options?: unknown) => {});
mock.module('sonner', () => ({ toast: { success() {}, error() {}, info() {}, warning } }));

let useRecordingStop: typeof import('../../src/hooks/useRecordingStop').useRecordingStop;
beforeAll(async () => {
  originals.recordingState = { ...await import('../../src/contexts/RecordingStateContext') };
  mock.module('../../src/contexts/RecordingStateContext', () => ({
    ...originals.recordingState,
    useRecordingState: () => ({ status: 'idle', setStatus() {}, isStopping: false, isProcessing: false, isSaving: false }),
  }));
  ({ useRecordingStop } = await import('../../src/hooks/useRecordingStop'));
});

async function resolveAllPending() {
  while (pending.length) {
    await act(async () => {
      for (const r of pending.splice(0)) r.resolve();
    });
  }
}

describe('useRecordingStop recording-stopped listener', () => {
  test('a re-render or unmount while listen() is pending leaks no listener', async () => {
    function Probe({ n }: { n: number }) {
      useRecordingStop(() => {}, () => {});
      return <>{n}</>;
    }
    let renderer: ReactTestRenderer | undefined;
    await act(async () => { renderer = create(<Probe n={1} />); });
    await act(async () => renderer!.update(<Probe n={2} />));
    await resolveAllPending();

    await act(async () => {
      for (const r of registrations.filter(r => r.name === 'recording-stopped' && !r.unlistened)) {
        r.handler({ payload: { message: 'stopped', audio_save_error: 'disk full' } });
      }
    });
    expect(warning).toHaveBeenCalledTimes(1);

    await act(async () => renderer!.unmount());
    await resolveAllPending();
    expect(registrations.length).toBeGreaterThan(0);
    expect(registrations.filter(r => !r.unlistened).map(r => r.name)).toEqual([]);
  });
});
