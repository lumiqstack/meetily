import { afterAll, beforeEach, describe, expect, mock, test } from 'bun:test';
import { act, create, type ReactTestRenderer } from 'react-test-renderer';

// Saving the summary must update the state that later saves read.
const originalCore = { ...await import('@tauri-apps/api/core') };
const originalSidebar = { ...await import('../../src/components/Sidebar/SidebarProvider') };
const originalSonner = { ...await import('sonner') };
afterAll(() => {
  mock.module('@tauri-apps/api/core', () => originalCore);
  mock.module('../../src/components/Sidebar/SidebarProvider', () => originalSidebar);
  mock.module('sonner', () => originalSonner);
});

const invoke = mock(async (_command: string, _args?: Record<string, unknown>): Promise<unknown> => undefined);
mock.module('@tauri-apps/api/core', () => ({ invoke }));
mock.module('sonner', () => ({ toast: { success: () => {}, error: () => {}, info: () => {}, warning: () => {} } }));
mock.module('../../src/components/Sidebar/SidebarProvider', () => ({
  useSidebar: () => ({ setCurrentMeeting: () => {}, setMeetings: () => {}, meetings: [] }),
}));
const { useMeetingData } = await import('../../src/hooks/meeting-details/useMeetingData');

type HookValue = ReturnType<typeof useMeetingData>;
let current: HookValue;
function Probe(props: { meeting: { id: string; title?: string }; summaryData: unknown }) {
  current = useMeetingData({ meeting: props.meeting, summaryData: props.summaryData as any });
  return null;
}

let renderer: ReactTestRenderer | undefined;
beforeEach(() => {
  invoke.mockClear();
  renderer = undefined;
});
async function render(meeting: { id: string; title?: string }, summaryData: unknown) {
  await act(async () => {
    if (renderer) renderer.update(<Probe meeting={meeting} summaryData={summaryData} />);
    else renderer = create(<Probe meeting={meeting} summaryData={summaryData} />);
  });
}
const savedMarkdown = () => invoke.mock.calls
  .filter(([command]) => command === 'api_save_meeting_summary')
  .map(([, args]) => (args as any).summary.markdown);

describe('saving the summary updates the state later saves read', () => {
  test('a second save with a clean editor writes the edited summary, not the stale one', async () => {
    await render({ id: 'meeting-a', title: 'A' }, { markdown: 'old' });

    // First save: the editor is dirty and saves its edited content through handleSaveSummary.
    current.blockNoteSummaryRef.current = {
      isDirty: true,
      saveSummary: () => current.handleSaveSummary({ markdown: 'new' } as any),
      getMarkdown: async () => '',
    } as any;
    await act(async () => { await current.saveAllChanges(); });
    expect(savedMarkdown()).toEqual(['new']);

    // Second save: the editor is no longer dirty, so the hook falls back to aiSummary.
    current.blockNoteSummaryRef.current = { ...current.blockNoteSummaryRef.current!, isDirty: false } as any;
    await act(async () => { await current.saveAllChanges(); });

    expect(savedMarkdown()).toEqual(['new', 'new']);
  });
});
