import { afterAll, describe, expect, mock, test } from 'bun:test';
import { act, create } from 'react-test-renderer';

const originalCore = { ...await import('@tauri-apps/api/core') };
const originalEvent = { ...await import('@tauri-apps/api/event') };
const originalNavigation = { ...await import('next/navigation') };
const originalAnalytics = { ...await import('../../src/lib/analytics') };
afterAll(() => {
  mock.module('@tauri-apps/api/core', () => originalCore);
  mock.module('@tauri-apps/api/event', () => originalEvent);
  mock.module('next/navigation', () => originalNavigation);
  mock.module('../../src/lib/analytics', () => originalAnalytics);
});

mock.module('next/navigation', () => ({ usePathname: () => '/meeting-details', useRouter: () => ({ push() {} }) }));
mock.module('../../src/contexts/RecordingStateContext', () => ({ useRecordingState: () => ({ isRecording: false }) }));
mock.module('../../src/lib/analytics', () => ({ default: { trackBackendConnection() {}, trackPageView() {} } }));

const handlers = new Map<string, (event: { payload: unknown }) => void>();
mock.module('@tauri-apps/api/event', () => ({
  ...originalEvent,
  listen: async (name: string, handler: (event: { payload: unknown }) => void) => {
    handlers.set(name, handler);
    return () => { if (handlers.get(name) === handler) handlers.delete(name); };
  },
}));

let backendMeetings = [
  { id: 'recorded', title: 'Recorded', transcribed: false, summarized: false, obsidian_exported: false },
  { id: 'transcribed', title: 'Transcribed', transcribed: true, summarized: false, obsidian_exported: false },
  { id: 'done', title: 'Done', transcribed: true, summarized: true, obsidian_exported: true },
];
let listFetches = 0;
mock.module('@tauri-apps/api/core', () => ({
  ...originalCore,
  invoke: async (command: string) => {
    if (command === 'api_get_meetings') {
      listFetches += 1;
      return backendMeetings;
    }
    throw new Error(`Unexpected command: ${command}`);
  },
}));
const { SidebarProvider, useSidebar } = await import('../../src/components/Sidebar/SidebarProvider');
const { missingStage } = await import('../../src/components/Sidebar/meetingStages');

let context!: ReturnType<typeof useSidebar>;
function Probe() {
  context = useSidebar();
  return null;
}
const flush = () => act(async () => { await new Promise((resolve) => setTimeout(resolve, 0)); });
const shown = (stage: Parameters<typeof missingStage>[1]) =>
  missingStage(context.sidebarItems, stage)[0].children!.map(item => item.id);

describe('sidebar missing-stage filter', () => {
  test('shows meetings missing each stage and refreshes when a stage can have finished', async () => {
    let renderer: ReturnType<typeof create> | undefined;
    await act(async () => { renderer = create(<SidebarProvider><Probe /></SidebarProvider>); });
    await flush();

    expect(shown(null)).toEqual(['recorded', 'transcribed', 'done']);
    expect(shown('transcribed')).toEqual(['recorded']);
    expect(shown('summarized')).toEqual(['recorded', 'transcribed']);
    expect(shown('obsidianExported')).toEqual(['recorded', 'transcribed']);

    const working = { payload: { current: { meeting_id: 'transcribed', stage: 'summarize' } } };
    await act(async () => { handlers.get('pipeline-status')!(working); });
    await flush();
    const fetchesWhileWorking = listFetches;
    await act(async () => { handlers.get('pipeline-status')!(working); });
    await flush();
    expect(listFetches).toBe(fetchesWhileWorking);

    backendMeetings = backendMeetings.map(m => m.id === 'transcribed' ? { ...m, summarized: true } : m);
    await act(async () => { handlers.get('pipeline-status')!({ payload: { current: null } }); });
    await flush();
    expect(shown('summarized')).toEqual(['recorded']);

    backendMeetings = backendMeetings.map(m => m.id === 'recorded' ? { ...m, transcribed: true } : m);
    await act(async () => { handlers.get('retranscription-complete')!({ payload: {} }); });
    await flush();
    expect(shown('transcribed')).toEqual([]);

    await act(async () => { renderer!.unmount(); });
    expect(handlers.has('pipeline-status')).toBe(false);
    expect(handlers.has('retranscription-complete')).toBe(false);
  });
});
