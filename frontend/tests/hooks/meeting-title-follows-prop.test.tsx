import { afterAll, describe, expect, mock, test } from 'bun:test';
import { act, create, type ReactTestRenderer } from 'react-test-renderer';

// H7-F3: meetingTitle must follow later meeting.title props (a rename from the
// sidebar), or Save to Obsidian writes the old title.
const originalCore = { ...await import('@tauri-apps/api/core') };
const originalSidebar = { ...await import('../../src/components/Sidebar/SidebarProvider') };
const originalSonner = { ...await import('sonner') };
afterAll(() => {
  mock.module('@tauri-apps/api/core', () => originalCore);
  mock.module('../../src/components/Sidebar/SidebarProvider', () => originalSidebar);
  mock.module('sonner', () => originalSonner);
});

mock.module('@tauri-apps/api/core', () => ({ invoke: async () => undefined }));
mock.module('sonner', () => ({ toast: { success: () => {}, error: () => {}, info: () => {}, warning: () => {} } }));
mock.module('../../src/components/Sidebar/SidebarProvider', () => ({
  useSidebar: () => ({ setCurrentMeeting: () => {}, setMeetings: () => {}, meetings: [] }),
}));
const { useMeetingData } = await import('../../src/hooks/meeting-details/useMeetingData');

let current: ReturnType<typeof useMeetingData>;
function Probe(props: { meeting: { id: string; title?: string } }) {
  current = useMeetingData({ meeting: props.meeting, summaryData: null });
  return null;
}

describe('meeting title follows the meeting prop', () => {
  test('rerender with a renamed meeting exposes the new title', async () => {
    let renderer: ReactTestRenderer | undefined;
    await act(async () => {
      renderer = create(<Probe meeting={{ id: 'meeting-a', title: 'A' }} />);
    });
    expect(current.meetingTitle).toBe('A');

    await act(async () => {
      renderer!.update(<Probe meeting={{ id: 'meeting-a', title: 'B' }} />);
    });
    expect(current.meetingTitle).toBe('B');

    await act(async () => renderer!.unmount());
  });
});
