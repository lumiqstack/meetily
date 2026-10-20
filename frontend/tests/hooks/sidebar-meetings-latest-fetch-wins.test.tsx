import { afterAll, afterEach, describe, expect, mock, test } from 'bun:test';
import { useEffect } from 'react';
import { act, create, type ReactTestRenderer } from 'react-test-renderer';

// Overlapping meetings-changed refetches: the newest response wins, and a failed fetch keeps the last good list.
const originalCore = { ...await import('@tauri-apps/api/core') };
const originalEvent = { ...await import('@tauri-apps/api/event') };
const originalAnalytics = { ...await import('../../src/lib/analytics') };
const originalRecordingState = { ...await import('../../src/contexts/RecordingStateContext') };
const originalPreferences = { ...await import('../../src/lib/summary-language-preferences') };
const originalNavigation = { ...await import('next/navigation') };
afterAll(() => {
  mock.module('@tauri-apps/api/core', () => originalCore);
  mock.module('@tauri-apps/api/event', () => originalEvent);
  mock.module('../../src/lib/analytics', () => originalAnalytics);
  mock.module('../../src/contexts/RecordingStateContext', () => originalRecordingState);
  mock.module('../../src/lib/summary-language-preferences', () => originalPreferences);
  mock.module('next/navigation', () => originalNavigation);
});

mock.module('next/navigation', () => ({ usePathname: () => '/', useRouter: () => ({}) }));
mock.module('../../src/contexts/RecordingStateContext', () => ({ useRecordingState: () => ({ isRecording: false }) }));
mock.module('../../src/lib/analytics', () => ({ default: { trackBackendConnection() {}, trackButtonClick() {} } }));
mock.module('@tauri-apps/api/event', () => ({ listen: async () => () => {} }));
mock.module('../../src/lib/summary-language-preferences', () => ({
  readCachedDetectedSummaryLanguage: async () => null,
  detectAndCacheSummaryLanguage: async () => ({ language: 'en' }),
  readMeetingSummaryLanguage: async () => ({ language: 'en', storage: 'metadata' }),
}));

type Deferred = { resolve: (value: unknown) => void; reject: (error: unknown) => void };
const pendingFetches: Deferred[] = [];
const invoke = mock(async (command: string): Promise<unknown> => {
  if (command === 'api_get_meetings') {
    return new Promise((resolve, reject) => pendingFetches.push({ resolve, reject }));
  }
  throw new Error(`Unexpected command: ${command}`);
});
mock.module('@tauri-apps/api/core', () => ({ invoke }));

const { SidebarProvider, useSidebar } = await import('../../src/components/Sidebar/SidebarProvider');

const row = (id: string, title: string) => ({ id, title, transcribed: true, summarized: false, obsidian_exported: false });

let refetch: () => Promise<void>;
let meetings: { id: string; title: string }[] = [];
function Consumer() {
  const sidebar = useSidebar();
  useEffect(() => {
    refetch = sidebar.refetchMeetings;
  }, [sidebar.refetchMeetings]);
  meetings = sidebar.meetings;
  return null;
}

let renderer: ReactTestRenderer | undefined;
afterEach(async () => {
  if (renderer) await act(async () => renderer!.unmount());
  renderer = undefined;
  pendingFetches.length = 0;
});

async function mountWithInitialList(list: unknown[]) {
  await act(async () => {
    renderer = create(<SidebarProvider><Consumer /></SidebarProvider>);
  });
  expect(pendingFetches.length).toBe(1);
  await act(async () => {
    pendingFetches.shift()!.resolve(list);
  });
}

describe('meetings list fetch ordering', () => {
  test('an older fetch resolving after a newer one does not overwrite the newer list', async () => {
    await mountWithInitialList([]);

    await act(async () => {
      void refetch();
      void refetch();
    });
    expect(pendingFetches.length).toBe(2);

    const [olderFetch, newerFetch] = pendingFetches;
    await act(async () => {
      newerFetch.resolve([row('m1', 'Renamed standup'), row('m2', 'New import')]);
    });
    await act(async () => {
      olderFetch.resolve([row('m1', 'Standup')]);
    });

    expect(meetings.map((m) => [m.id, m.title])).toEqual([
      ['m1', 'Renamed standup'],
      ['m2', 'New import'],
    ]);
  });

  test('a failed fetch keeps the last good list', async () => {
    await mountWithInitialList([row('m1', 'Standup')]);
    expect(meetings.map((m) => m.id)).toEqual(['m1']);

    await act(async () => {
      void refetch();
    });
    await act(async () => {
      pendingFetches.shift()!.reject(new Error('database is locked'));
    });

    expect(meetings.map((m) => m.id)).toEqual(['m1']);
  });
});
