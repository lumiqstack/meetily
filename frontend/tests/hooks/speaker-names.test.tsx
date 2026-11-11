import { afterAll, beforeEach, describe, expect, mock, test } from 'bun:test';
import { act, create } from 'react-test-renderer';

const originalCore = { ...await import('@tauri-apps/api/core') };
const originalToast = { ...await import('sonner') };
afterAll(() => {
  mock.module('@tauri-apps/api/core', () => originalCore);
  mock.module('sonner', () => originalToast);
});

const success = mock((_title: string, _options?: { description?: string }) => {});
const error = mock((_title: string, _options?: { description?: string }) => {});
mock.module('sonner', () => ({ toast: { success, error, info() {}, warning() {} } }));

let stored = [
  { speaker: 'Speaker 1', segment_count: 42, first_start: 0 },
  { speaker: 'mic', segment_count: 5, first_start: 3 },
];
let renameResult: () => Promise<number> = async () => 42;
const invoke = mock(async (command: string, _args?: Record<string, unknown>): Promise<unknown> => {
  if (command === 'api_get_meeting_speakers') return stored;
  if (command === 'api_rename_meeting_speaker') return renameResult();
  throw new Error(`Unexpected command: ${command}`);
});
mock.module('@tauri-apps/api/core', () => ({ ...originalCore, invoke }));

const { useSpeakerNames } = await import('../../src/hooks/meeting-details/useSpeakerNames');

type Hook = ReturnType<typeof useSpeakerNames>;
let hook: Hook;
const renamed = mock((_old: string, _new: string) => {});

function Probe({ hasSummary }: { hasSummary: boolean }) {
  hook = useSpeakerNames({ meetingId: 'm1', hasSummary, onRenamed: renamed });
  return null;
}

async function mount(hasSummary = false) {
  await act(async () => {
    create(<Probe hasSummary={hasSummary} />);
  });
}

const renameCalls = () => invoke.mock.calls.filter(([command]) => command === 'api_rename_meeting_speaker');

beforeEach(() => {
  invoke.mockClear();
  success.mockClear();
  error.mockClear();
  renamed.mockClear();
  renameResult = async () => 42;
  stored = [
    { speaker: 'Speaker 1', segment_count: 42, first_start: 0 },
    { speaker: 'mic', segment_count: 5, first_start: 3 },
  ];
});

describe('useSpeakerNames', () => {
  test('loads the meeting speakers', async () => {
    await mount();
    expect(hook.speakers.map((s) => s.speaker)).toEqual(['Speaker 1', 'mic']);
  });

  test('renames by raw value, updates loaded segments, refreshes and toasts', async () => {
    await mount();
    stored = [
      { speaker: 'Ana', segment_count: 42, first_start: 0 },
      { speaker: 'mic', segment_count: 5, first_start: 3 },
    ];
    let ok = false;
    await act(async () => {
      ok = await hook.rename('Speaker 1', ' Ana ');
    });

    expect(ok).toBe(true);
    expect(renameCalls()[0][1]).toEqual({ meetingId: 'm1', oldSpeaker: 'Speaker 1', newSpeaker: 'Ana' });
    expect(renamed).toHaveBeenCalledWith('Speaker 1', 'Ana');
    expect(hook.speakers[0].speaker).toBe('Ana');
    expect(success.mock.calls[0][0]).toBe('Renamed Speaker 1 to Ana in 42 segments');
    expect(success.mock.calls[0][1]?.description).toBeUndefined();
  });

  test('typing a source tag label merges into its raw value', async () => {
    await mount();
    await act(async () => {
      await hook.rename('Speaker 1', 'me');
    });
    expect(renameCalls()[0][1]).toMatchObject({ oldSpeaker: 'Speaker 1', newSpeaker: 'mic' });
    expect(success.mock.calls[0][0]).toBe('Merged Speaker 1 into Me (42 segments)');
  });

  test('mentions the stale summary when one exists', async () => {
    await mount(true);
    await act(async () => {
      await hook.rename('mic', 'Marcos');
    });
    expect(success.mock.calls[0][1]?.description).toContain('"Me"');
  });

  test('does not call the backend for an unchanged or empty name', async () => {
    await mount();
    let unchanged = false;
    let empty = true;
    await act(async () => {
      unchanged = await hook.rename('mic', 'Me');
      empty = await hook.rename('mic', '   ');
    });
    expect(unchanged).toBe(true);
    expect(empty).toBe(false);
    expect(renameCalls()).toHaveLength(0);
    expect(renamed).not.toHaveBeenCalled();
  });

  test('shows the backend error and leaves segments alone', async () => {
    await mount();
    renameResult = async () => {
      throw 'Speaker name must be at most 80 characters';
    };
    let ok = true;
    await act(async () => {
      ok = await hook.rename('Speaker 1', 'Ana');
    });
    expect(ok).toBe(false);
    expect(renamed).not.toHaveBeenCalled();
    expect(error.mock.calls[0][1]?.description).toBe('Speaker name must be at most 80 characters');
  });
});
