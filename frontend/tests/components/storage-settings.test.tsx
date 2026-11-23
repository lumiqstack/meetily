import { afterAll, beforeEach, describe, expect, mock, test } from 'bun:test';
import { act, create, type ReactTestInstance, type ReactTestRenderer } from 'react-test-renderer';
import type { ReactNode } from 'react';

const originalCore = { ...await import('@tauri-apps/api/core') };
const originalEvent = { ...await import('@tauri-apps/api/event') };
const originalToast = { ...await import('sonner') };
const originalDialog = { ...await import('../../src/components/ui/dialog') };
afterAll(() => {
  mock.module('@tauri-apps/api/core', () => originalCore);
  mock.module('@tauri-apps/api/event', () => originalEvent);
  mock.module('sonner', () => originalToast);
  mock.module('../../src/components/ui/dialog', () => originalDialog);
});

// Radix portals need a DOM; the test renderer has none, so render dialog parts inline.
const Pass = ({ children }: { children?: ReactNode }) => <div>{children}</div>;
mock.module('../../src/components/ui/dialog', () => ({
  Dialog: ({ open, children }: { open: boolean; children?: ReactNode }) => (open ? <div data-dialog>{children}</div> : null),
  DialogContent: Pass,
  DialogHeader: Pass,
  DialogFooter: Pass,
  DialogTitle: Pass,
  DialogDescription: Pass,
}));

const toastSuccess = mock(() => {});
mock.module('sonner', () => ({ toast: { success: toastSuccess, error() {}, info() {}, warning() {} } }));

const handlers = new Map<string, () => void>();
mock.module('@tauri-apps/api/event', () => ({
  ...originalEvent,
  listen: async (name: string, handler: () => void) => {
    handlers.set(name, handler);
    return () => handlers.delete(name);
  },
}));

const MB = 1024 * 1024;
const survey = {
  meetings: [
    { meeting_id: 'done', title: 'Weekly sync', created_at: '2026-01-10T10:00:00Z', audio: { state: 'removable', bytes: 40 * MB } },
    {
      meeting_id: 'pending',
      title: 'Not transcribed',
      created_at: '2026-01-11T10:00:00Z',
      audio: { state: 'held', bytes: 10 * MB, reason: { code: 'not_transcribed', text: 'Not transcribed yet. The audio is the only copy.' } },
    },
    { meeting_id: 'other', title: 'Planning', created_at: '2026-01-12T10:00:00Z', audio: { state: 'removable', bytes: 8 * MB } },
  ],
  orphans: [],
  volumes: [{ mount_point: '/', free_bytes: 5 * 1024 * MB, total_bytes: 100 * 1024 * MB, audio_bytes: 58 * MB }],
  audio_bytes: 58 * MB,
};

const calls: { command: string; args?: Record<string, unknown> }[] = [];
const invoke = mock(async (command: string, args?: Record<string, unknown>): Promise<unknown> => {
  calls.push({ command, args });
  if (command === 'recordings_survey') return survey;
  if (command === 'recordings_purge') {
    return { freed_bytes: 40 * MB, purged_meetings: ['done'], purged_orphans: 0, skipped: [], failed: [] };
  }
  throw new Error(`Unexpected command: ${command}`);
});
mock.module('@tauri-apps/api/core', () => ({ ...originalCore, invoke }));
const { StorageSettings } = await import('../../src/components/Storage/StorageSettings');

const flush = () => act(async () => { await new Promise((resolve) => setTimeout(resolve, 0)); });

const textOf = (node: ReactTestInstance | string): string =>
  typeof node === 'string' ? node : node.children.map(textOf).join('');
const checkboxes = (r: ReactTestRenderer) => r.root.findAll((n) => n.type === 'input' && n.props.type === 'checkbox');
const buttonWith = (r: ReactTestRenderer, label: string) =>
  r.root.findAll((n) => n.type === 'button' && textOf(n).includes(label))[0];

async function render() {
  let renderer!: ReactTestRenderer;
  await act(async () => { renderer = create(<StorageSettings />); });
  await flush();
  return renderer;
}

describe('Storage settings', () => {
  beforeEach(() => {
    calls.length = 0;
    toastSuccess.mockClear();
  });

  test('held rows show the reason and offer no checkbox', async () => {
    const r = await render();
    const labels = checkboxes(r).map((c) => c.props['aria-label']);
    expect(labels).toEqual(['Select Weekly sync', 'Select Planning']);
    expect(textOf(r.root)).toContain('Not transcribed yet. The audio is the only copy.');
    expect(textOf(r.root)).toContain('Recordings use 58.0 MB');
    expect(textOf(r.root)).toContain('5.0 GB free of 100.0 GB on /');
  });

  test('confirming states the count and bytes and purges by id only', async () => {
    const r = await render();
    await act(async () => { buttonWith(r, '30 days').props.onClick(); });
    expect(checkboxes(r).map((c) => c.props.checked)).toEqual([true, true]);

    await act(async () => { checkboxes(r)[1].props.onChange({ target: { checked: false } }); });
    await act(async () => { buttonWith(r, 'Remove selected audio').props.onClick(); });

    const dialog = r.root.find((n) => n.props['data-dialog'] === true);
    expect(textOf(dialog)).toContain('Permanently remove the audio of 1 meeting? This frees 40.0 MB.');
    expect(textOf(dialog)).toContain('Transcripts and summaries are kept.');

    calls.length = 0;
    await act(async () => { await buttonWith(r, 'Remove audio').props.onClick(); });
    await flush();

    expect(calls[0]).toEqual({
      command: 'recordings_purge',
      args: { request: { meetingIds: ['done'], orphanFolders: [] } },
    });
    expect(calls.some((c) => c.command === 'recordings_survey')).toBe(true);
    expect(toastSuccess).toHaveBeenCalledTimes(1);
    expect(r.root.findAll((n) => n.props['data-dialog'] === true)).toHaveLength(0);
  });

  test('refreshes when meetings change', async () => {
    await render();
    calls.length = 0;
    await act(async () => { handlers.get('meetings-changed')!(); });
    await flush();
    expect(calls.map((c) => c.command)).toEqual(['recordings_survey']);
  });
});
