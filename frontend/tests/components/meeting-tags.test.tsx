import { afterAll, beforeEach, describe, expect, mock, test } from 'bun:test';
import { act, create, type ReactTestRenderer } from 'react-test-renderer';

const originalCore = { ...await import('@tauri-apps/api/core') };
const originalToast = { ...await import('sonner') };
afterAll(() => {
  mock.module('@tauri-apps/api/core', () => originalCore);
  mock.module('sonner', () => originalToast);
});

const toastError = mock(() => {});
mock.module('sonner', () => ({ toast: { error: toastError, success() {}, info() {}, warning() {} } }));

let storedTags: Record<string, string[]>;
let failSave = false;
const invoke = mock(async (command: string, args?: Record<string, unknown>): Promise<unknown> => {
  if (command === 'api_get_meeting_tags') return storedTags[args!.meetingId as string] ?? [];
  if (command === 'api_get_all_meeting_tags') return ['clients', 'q4', 'internal'];
  if (command === 'api_set_meeting_tags') {
    if (failSave) throw new Error('db locked');
    // Mirror the Rust normalization closely enough for the UI contract.
    const tags = (args!.tags as string[]).map((t) => t.trim().replace(/^#/, '').replace(/\s+/g, '-').toLowerCase());
    storedTags[args!.meetingId as string] = tags;
    return tags;
  }
  throw new Error(`Unexpected command: ${command}`);
});
mock.module('@tauri-apps/api/core', () => ({ ...originalCore, invoke }));
const { MeetingTags } = await import('../../src/components/MeetingDetails/MeetingTags');

const flush = () => act(async () => { await new Promise((resolve) => setTimeout(resolve, 0)); });

async function render(meetingId = 'meeting-a') {
  let renderer!: ReactTestRenderer;
  await act(async () => { renderer = create(<MeetingTags meetingId={meetingId} />); });
  await flush();
  return renderer;
}

const chips = (r: ReactTestRenderer) =>
  r.root.findAll((node) => node.type === 'span' && typeof node.props.className === 'string' && node.props.className.includes('rounded-full'))
    .map((node) => node.children.find((child) => typeof child === 'string'));
const input = (r: ReactTestRenderer) => r.root.findByType('input');

async function type(r: ReactTestRenderer, value: string, key = 'Enter') {
  await act(async () => { input(r).props.onChange({ target: { value } }); });
  await act(async () => { input(r).props.onKeyDown({ key, preventDefault() {} }); });
  await flush();
}

describe('MeetingTags', () => {
  beforeEach(() => {
    storedTags = { 'meeting-a': ['clients'] };
    failSave = false;
    invoke.mockClear();
    toastError.mockClear();
  });

  test('loads the meeting tags and offers unused existing tags as suggestions', async () => {
    const r = await render();
    expect(chips(r)).toEqual(['clients']);
    const options = r.root.findAllByType('option').map((o) => o.props.value);
    expect(options).toEqual(['q4', 'internal']);
  });

  test('Enter adds a tag and stores the backend-normalized value', async () => {
    const r = await render();
    await type(r, '#Mercado Libre');
    expect(invoke).toHaveBeenCalledWith('api_set_meeting_tags', { meetingId: 'meeting-a', tags: ['clients', '#Mercado Libre'] });
    expect(chips(r)).toEqual(['clients', 'mercado-libre']);
    expect(input(r).props.value).toBe('');
  });

  test('comma-separated input adds several tags at once, skipping duplicates', async () => {
    const r = await render();
    await act(async () => { input(r).props.onChange({ target: { value: 'q4, Clients, internal' } }); });
    await act(async () => { input(r).props.onBlur(); });
    await flush();
    expect(storedTags['meeting-a']).toEqual(['clients', 'q4', 'internal']);
  });

  test('the remove button and Backspace on an empty field drop tags', async () => {
    storedTags['meeting-a'] = ['clients', 'q4', 'internal'];
    const r = await render();
    const remove = r.root.findByProps({ 'aria-label': 'Remove tag q4' });
    await act(async () => { remove.props.onClick({ stopPropagation() {} }); });
    await flush();
    expect(chips(r)).toEqual(['clients', 'internal']);

    await act(async () => { input(r).props.onKeyDown({ key: 'Backspace', preventDefault() {} }); });
    await flush();
    expect(chips(r)).toEqual(['clients']);
  });

  test('a failed save restores the previous tags and tells the user', async () => {
    const r = await render();
    failSave = true;
    await type(r, 'q4');
    expect(chips(r)).toEqual(['clients']);
    expect(toastError).toHaveBeenCalledTimes(1);
  });

  test('switching meetings reloads that meeting\'s tags', async () => {
    storedTags['meeting-b'] = ['internal'];
    const r = await render('meeting-a');
    await act(async () => { r.update(<MeetingTags meetingId="meeting-b" />); });
    await flush();
    expect(chips(r)).toEqual(['internal']);
  });
});
