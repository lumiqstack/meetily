import { afterAll, beforeEach, describe, expect, mock, test } from 'bun:test';
import { act, create, type ReactTestRenderer } from 'react-test-renderer';

// Unsaved summary edits must be saved when the summary editor unmounts (for example when
// navigating to another meeting), through the same save path the Save button uses.
const originalCore = { ...await import('@tauri-apps/api/core') };
const originalSidebar = { ...await import('../../src/components/Sidebar/SidebarProvider') };
const originalSonner = { ...await import('sonner') };
const originalDynamic = { ...await import('next/dynamic') };
const originalBlockNoteReact = { ...await import('@blocknote/react') };
const originalBlockNoteShadcn = { ...await import('@blocknote/shadcn') };
afterAll(() => {
  mock.module('@tauri-apps/api/core', () => originalCore);
  mock.module('../../src/components/Sidebar/SidebarProvider', () => originalSidebar);
  mock.module('sonner', () => originalSonner);
  mock.module('next/dynamic', () => originalDynamic);
  mock.module('@blocknote/react', () => originalBlockNoteReact);
  mock.module('@blocknote/shadcn', () => originalBlockNoteShadcn);
});

const invoke = mock(async (_command: string, _args?: Record<string, unknown>): Promise<unknown> => undefined);
const toastError = mock((_message: string, _options?: unknown) => {});
mock.module('@tauri-apps/api/core', () => ({ invoke }));
mock.module('sonner', () => ({ toast: { success: () => {}, error: toastError, info: () => {}, warning: () => {} } }));
mock.module('../../src/components/Sidebar/SidebarProvider', () => ({
  useSidebar: () => ({ setCurrentMeeting: () => {}, setMeetings: () => {}, meetings: [] }),
}));

// Stands in for the BlockNote editor. The test drives its onChange directly.
let emitEditorChange: ((blocks: unknown[]) => void) | undefined;
function FakeEditor(props: { onChange: (blocks: unknown[]) => void }) {
  emitEditorChange = props.onChange;
  return null;
}
mock.module('next/dynamic', () => ({ default: () => FakeEditor }));

const fakeBlockNoteEditor = {
  document: [],
  tryParseMarkdownToBlocks: async () => [],
  replaceBlocks: () => {},
  blocksToMarkdownLossy: async (_blocks: unknown[]) => 'edited markdown',
};
mock.module('@blocknote/react', () => ({ useCreateBlockNote: () => fakeBlockNoteEditor }));
mock.module('@blocknote/shadcn', () => ({ BlockNoteView: () => null }));

const { useMeetingData } = await import('../../src/hooks/meeting-details/useMeetingData');
const { BlockNoteSummaryView } = await import('../../src/components/AISummary/BlockNoteSummaryView');

const summaryBlocks = [{ id: 'b1', type: 'paragraph', props: {}, content: [{ type: 'text', text: 'old', styles: {} }], children: [] }];

function Summary(props: { meetingId: string; summaryData: unknown }) {
  const meetingData = useMeetingData({
    meeting: { id: props.meetingId, title: 'A' },
    summaryData: props.summaryData as any,
  });
  return (
    <BlockNoteSummaryView
      ref={meetingData.blockNoteSummaryRef}
      summaryData={meetingData.aiSummary}
      onSave={meetingData.handleSaveSummary as any}
      meeting={{ id: props.meetingId, title: 'A', created_at: '2026-10-07T00:00:00Z' }}
    />
  );
}

let renderer: ReactTestRenderer | undefined;
beforeEach(() => {
  invoke.mockClear();
  toastError.mockClear();
  renderer = undefined;
  emitEditorChange = undefined;
});

async function mountSummary() {
  await act(async () => {
    renderer = create(<Summary meetingId="meeting-a" summaryData={{ summary_json: summaryBlocks }} />);
  });
  // The editor ignores changes until the loaded content has settled (100ms).
  await act(async () => { await new Promise((resolve) => setTimeout(resolve, 150)); });
}

async function editSummary() {
  await act(async () => {
    emitEditorChange!([{ ...summaryBlocks[0], content: [{ type: 'text', text: 'edited', styles: {} }] }]);
  });
}

const savedMarkdown = () => invoke.mock.calls
  .filter(([command]) => command === 'api_save_meeting_summary')
  .map(([, args]) => (args as any).summary.markdown);

const settle = () => new Promise((resolve) => setTimeout(resolve, 20));

describe('summary editor saves unsaved edits when it unmounts', () => {
  test('unmounting a dirty summary editor saves its edited markdown', async () => {
    await mountSummary();
    await editSummary();

    await act(async () => { renderer!.unmount(); });
    await settle();

    expect(savedMarkdown()).toEqual(['edited markdown']);
    expect(toastError).not.toHaveBeenCalled();
  });

  test('a failed autosave on unmount is reported through a toast', async () => {
    invoke.mockImplementationOnce(async () => { throw new Error('disk full'); });
    await mountSummary();
    await editSummary();

    await act(async () => { renderer!.unmount(); });
    await settle();

    expect(toastError).toHaveBeenCalledTimes(1);
    expect(toastError.mock.calls[0]?.[0]).toBe('Failed to save changes');
  });

  test('an editor change the user did not make is not autosaved', async () => {
    // BlockNote can report a change while it normalizes loaded content; saving that
    // would rewrite an untouched summary through the lossy markdown conversion.
    await mountSummary();
    await act(async () => {
      emitEditorChange!([{ ...summaryBlocks[0], content: [{ type: 'text', text: 'normalized', styles: {} }] }]);
    });

    await act(async () => { renderer!.unmount(); });
    await settle();

    expect(savedMarkdown()).toEqual([]);
  });

  test('unmounting a clean summary editor does not save', async () => {
    await mountSummary();

    await act(async () => { renderer!.unmount(); });
    await settle();

    expect(savedMarkdown()).toEqual([]);
  });
});
