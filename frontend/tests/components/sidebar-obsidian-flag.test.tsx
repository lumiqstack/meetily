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
    return () => handlers.delete(name);
  },
}));
let exported = new Set(['meeting-a']);
mock.module('@tauri-apps/api/core', () => ({
  ...originalCore,
  invoke: async (command: string) => {
    if (command === 'api_get_meetings') {
      return [
        { id: 'meeting-a', title: 'Exported', obsidian_exported: exported.has('meeting-a') },
        { id: 'meeting-b', title: 'Not yet', obsidian_exported: exported.has('meeting-b') },
      ];
    }
    throw new Error(`Unexpected command: ${command}`);
  },
}));
const { SidebarProvider, useSidebar } = await import('../../src/components/Sidebar/SidebarProvider');

let context!: ReturnType<typeof useSidebar>;
function Probe() {
  context = useSidebar();
  return null;
}
const flush = () => act(async () => { await new Promise((resolve) => setTimeout(resolve, 0)); });
const flags = () =>
  Object.fromEntries(context.sidebarItems[0].children!.map((item) => [item.id, item.obsidianExported]));

describe('sidebar Obsidian indicator', () => {
  test('carries the exported flag from the backend and refetches it when meetings change', async () => {
    let renderer: ReturnType<typeof create> | undefined;
    await act(async () => { renderer = create(<SidebarProvider><Probe /></SidebarProvider>); });
    await flush();
    expect(flags()).toEqual({ 'meeting-a': true, 'meeting-b': false });

    exported.add('meeting-b');
    await act(async () => { handlers.get('meetings-changed')!({ payload: { meeting_id: 'meeting-b' } }); });
    await flush();
    expect(flags()).toEqual({ 'meeting-a': true, 'meeting-b': true });

    await act(async () => { renderer!.unmount(); });
    expect(handlers.has('meetings-changed')).toBe(false);
  });
});
