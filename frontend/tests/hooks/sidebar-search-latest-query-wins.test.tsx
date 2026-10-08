import { afterAll, afterEach, describe, expect, mock, test } from 'bun:test';
import { useEffect } from 'react';
import { act, create, type ReactTestRenderer } from 'react-test-renderer';

// H7-F4: an older search response resolving last must not replace the latest query's results.
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

type Deferred = { query: string; resolve: (value: unknown) => void };
const pendingSearches: Deferred[] = [];
const invoke = mock(async (command: string, args?: Record<string, unknown>): Promise<unknown> => {
  if (command === 'api_get_meetings') return [];
  if (command === 'api_search_transcripts') {
    return new Promise((resolve) => pendingSearches.push({ query: args!.query as string, resolve }));
  }
  throw new Error(`Unexpected command: ${command}`);
});
mock.module('@tauri-apps/api/core', () => ({ invoke }));

const { SidebarProvider, useSidebar } = await import('../../src/components/Sidebar/SidebarProvider');

let search: (query: string) => Promise<void>;
let searchResults: unknown[] = [];
function Consumer() {
  const sidebar = useSidebar();
  useEffect(() => {
    search = sidebar.searchTranscripts;
  }, [sidebar.searchTranscripts]);
  searchResults = sidebar.searchResults;
  return null;
}

let renderer: ReactTestRenderer | undefined;
afterEach(async () => {
  if (renderer) await act(async () => renderer!.unmount());
  renderer = undefined;
  pendingSearches.length = 0;
});

describe('transcript search ordering', () => {
  test('a stale "b" response resolving after "budget" does not overwrite the budget results', async () => {
    await act(async () => {
      renderer = create(<SidebarProvider><Consumer /></SidebarProvider>);
    });

    await act(async () => {
      void search('b');
      void search('budget');
    });
    expect(pendingSearches.map((p) => p.query)).toEqual(['b', 'budget']);

    // The newer query answers first, then the older query answers last.
    const [bSearch, budgetSearch] = pendingSearches;
    await act(async () => {
      budgetSearch.resolve([{ id: 'budget-meeting', title: 'Budget review', matchContext: 'budget', timestamp: '00:01' }]);
    });
    await act(async () => {
      bSearch.resolve([{ id: 'bob-meeting', title: 'Bob sync', matchContext: 'b', timestamp: '00:02' }]);
    });

    // Expected: the results shown belong to the latest query ("budget").
    expect(searchResults.map((r: any) => r.id)).toEqual(['budget-meeting']);
  });
});
