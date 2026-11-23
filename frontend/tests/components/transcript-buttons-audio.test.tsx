import { afterAll, beforeEach, describe, expect, mock, test } from 'bun:test';
import { act, create, type ReactTestInstance, type ReactTestRenderer } from 'react-test-renderer';
import type { ReactNode } from 'react';

const originalCore = { ...await import('@tauri-apps/api/core') };
const originalEvent = { ...await import('@tauri-apps/api/event') };
const originalToast = { ...await import('sonner') };
const originalDialog = { ...await import('../../src/components/ui/dialog') };
const originalConfig = { ...await import('../../src/contexts/ConfigContext') };
const originalAnalytics = { ...await import('../../src/lib/analytics') };
const originalRetranscribe = { ...await import('../../src/components/MeetingDetails/RetranscribeDialog') };
const hadWindow = 'window' in globalThis;
(globalThis as any).window ??= { addEventListener() {}, removeEventListener() {} };
afterAll(() => {
  if (!hadWindow) delete (globalThis as any).window;
  mock.module('@tauri-apps/api/core', () => originalCore);
  mock.module('@tauri-apps/api/event', () => originalEvent);
  mock.module('sonner', () => originalToast);
  mock.module('../../src/components/ui/dialog', () => originalDialog);
  mock.module('../../src/contexts/ConfigContext', () => originalConfig);
  mock.module('../../src/lib/analytics', () => originalAnalytics);
  mock.module('../../src/components/MeetingDetails/RetranscribeDialog', () => originalRetranscribe);
});

const Pass = ({ children }: { children?: ReactNode }) => <div>{children}</div>;
mock.module('../../src/components/ui/dialog', () => ({
  Dialog: ({ open, children }: { open: boolean; children?: ReactNode }) => (open ? <div data-dialog>{children}</div> : null),
  DialogContent: Pass,
  DialogHeader: Pass,
  DialogFooter: Pass,
  DialogTitle: Pass,
  DialogDescription: Pass,
}));
mock.module('../../src/components/MeetingDetails/RetranscribeDialog', () => ({
  RetranscribeDialog: ({ open }: { open: boolean }) => (open ? <div data-retranscribe /> : null),
}));
mock.module('../../src/contexts/ConfigContext', () => ({ useConfig: () => ({ betaFeatures: { importAndRetranscribe: true } }) }));
mock.module('../../src/lib/analytics', () => ({ default: { trackButtonClick() {} } }));
mock.module('sonner', () => ({ toast: { success() {}, error() {}, info() {}, warning() {} } }));
mock.module('@tauri-apps/api/event', () => ({ ...originalEvent, listen: async () => () => {} }));

const MB = 1024 * 1024;
let status: unknown;
const invoke = mock(async (command: string): Promise<unknown> => {
  if (command === 'recordings_audio_status') return status;
  if (command === 'recordings_purge') {
    status = { state: 'absent', reason: { code: 'no_audio_files', text: 'This meeting has no audio on this computer, so it cannot be enhanced.' } };
    return { freed_bytes: 48 * MB, purged_meetings: ['m1'], purged_orphans: 0, skipped: [], failed: [] };
  }
  throw new Error(`Unexpected command: ${command}`);
});
mock.module('@tauri-apps/api/core', () => ({ ...originalCore, invoke }));
const { TranscriptButtonGroup } = await import('../../src/components/MeetingDetails/TranscriptButtonGroup');

const flush = () => act(async () => { await new Promise((resolve) => setTimeout(resolve, 0)); });
const textOf = (node: ReactTestInstance | string): string =>
  typeof node === 'string' ? node : node.children.map(textOf).join('');
const button = (r: ReactTestRenderer, label: string) =>
  r.root.findAll((n) => n.type === 'button' && textOf(n).includes(label))[0];

async function render() {
  let renderer!: ReactTestRenderer;
  await act(async () => {
    renderer = create(
      <TranscriptButtonGroup
        transcriptCount={3}
        onCopyTranscript={() => {}}
        onOpenMeetingFolder={async () => {}}
        meetingId="m1"
        meetingFolderPath="/rec/m1"
      />,
    );
  });
  await flush();
  return renderer;
}

describe('meeting detail audio buttons', () => {
  beforeEach(() => invoke.mockClear());

  test('absent audio blocks Enhance with the reason and hides Remove audio', async () => {
    status = { state: 'absent', reason: { code: 'no_audio_files', text: 'This meeting has no audio on this computer, so it cannot be enhanced.' } };
    const r = await render();
    const enhance = button(r, 'Enhance');
    expect(enhance.props['aria-disabled']).toBe(true);
    expect(enhance.props.title).toBe('This meeting has no audio on this computer, so it cannot be enhanced.');
    await act(async () => { enhance.props.onClick(); });
    expect(r.root.findAll((n) => n.props['data-retranscribe'] === true)).toHaveLength(0);
    expect(button(r, 'Remove audio')).toBeUndefined();
  });

  test('held audio shows Remove audio blocked with the reason', async () => {
    status = { state: 'held', bytes: 48 * MB, reason: { code: 'not_transcribed', text: 'Not transcribed yet.' } };
    const r = await render();
    const remove = button(r, 'Remove audio');
    expect(remove.props['aria-disabled']).toBe(true);
    expect(remove.props.title).toBe('Not transcribed yet.');
    await act(async () => { remove.props.onClick(); });
    expect(r.root.findAll((n) => n.props['data-dialog'] === true)).toHaveLength(0);
    expect(button(r, 'Enhance').props['aria-disabled']).toBe(false);
  });

  test('removable audio enables both and removing it purges this meeting only', async () => {
    status = { state: 'removable', bytes: 48 * MB };
    const r = await render();
    expect(button(r, 'Enhance').props['aria-disabled']).toBe(false);
    const remove = button(r, 'Remove audio');
    expect(remove.props['aria-disabled']).toBe(false);
    expect(remove.props.title).toContain('48.0 MB');

    await act(async () => { remove.props.onClick(); });
    const dialog = r.root.find((n) => n.props['data-dialog'] === true);
    expect(textOf(dialog)).toContain('Permanently remove the audio of 1 meeting? This frees 48.0 MB.');

    const confirm = dialog.findAll((n) => n.type === 'button' && textOf(n) === 'Remove audio')[0];
    await act(async () => { await confirm.props.onClick(); });
    await flush();
    expect(invoke.mock.calls.find((c) => c[0] === 'recordings_purge')).toEqual([
      'recordings_purge',
      { request: { meetingIds: ['m1'], orphanFolders: [] } },
    ]);
    expect(button(r, 'Remove audio')).toBeUndefined();
    expect(button(r, 'Enhance').props['aria-disabled']).toBe(true);
  });
});
