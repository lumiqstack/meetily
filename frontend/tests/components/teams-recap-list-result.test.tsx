import { afterAll, describe, expect, mock, test } from 'bun:test';
import { act, create, type ReactTestRenderer, type ReactTestInstance } from 'react-test-renderer';

// Check recap list reports on a line beside its button that stays, not in a
// toast that goes. The line comes from the stored last listing, so it is back
// whenever Settings opens; a check that never reached the list says why there.
const originals = {
  core: { ...await import('@tauri-apps/api/core') },
  event: { ...await import('@tauri-apps/api/event') },
  sonner: { ...await import('sonner') },
};
afterAll(() => {
  mock.module('@tauri-apps/api/core', () => originals.core);
  mock.module('@tauri-apps/api/event', () => originals.event);
  mock.module('sonner', () => originals.sonner);
});

const toasts: string[] = [];
const toastOf = (kind: string) => (title: string) => {
  toasts.push(`${kind}: ${title}`);
};
mock.module('sonner', () => ({
  toast: { error: toastOf('error'), success: toastOf('success'), warning: toastOf('warning'), info: toastOf('info') },
}));
mock.module('@tauri-apps/api/event', () => ({ ...originals.event, listen: async () => () => undefined }));

let stored: unknown = null;
let check: () => Promise<unknown> = async () => ({});
const invoke = mock(async (command: string): Promise<unknown> => {
  if (command === 'get_teams_recap_sync_state_command') {
    return { sinceDate: null, lastSyncDate: null, knownRecaps: 0, available: true, lastListing: stored };
  }
  if (command === 'teams_recap_list_debug_command') return check();
  return undefined;
});
mock.module('@tauri-apps/api/core', () => ({ ...originals.core, invoke }));
const { TeamsRecapSyncSettings } = await import('../../src/components/TeamsRecapSyncSettings');

const readNow = (listed: number, synced: number) => ({
  readAt: new Date().toISOString(), windowStart: '2026-10-29T01:16:21+00:00',
  listed, synced, pending: listed - synced, notReady: 0, error: null, endedEarlyAt: null,
});

async function render(): Promise<ReactTestRenderer> {
  let renderer: ReactTestRenderer | undefined;
  await act(async () => {
    renderer = create(<TeamsRecapSyncSettings vaultConfigured />);
  });
  return renderer!;
}

const resultLine = (renderer: ReactTestRenderer): string | null => {
  const found = renderer.root.findAll((node) => node.props['data-testid'] === 'recap-list-result');
  return found.length ? found[0].children.join('') : null;
};

async function pressCheck(renderer: ReactTestRenderer) {
  const button = renderer.root.find(
    (node: ReactTestInstance) => node.type === 'button' && node.children.join('') === 'Check recap list',
  );
  await act(async () => {
    await button.props.onClick();
  });
}

describe('Check recap list result line', () => {
  test('shows the stored result when Settings opens', async () => {
    stored = readNow(28, 3);
    toasts.length = 0;
    const renderer = await render();
    expect(resultLine(renderer)).toMatch(/^Checked at .+: 28 recaps, 3 already synced, 25 pending\.$/);
    await act(async () => renderer.unmount());
  });

  test('a check puts what it read on the line, with no toast', async () => {
    stored = null;
    toasts.length = 0;
    const renderer = await render();
    expect(resultLine(renderer)).toBeNull();
    check = async () => {
      stored = readNow(28, 0);
      return {};
    };
    await pressCheck(renderer);
    expect(resultLine(renderer)).toMatch(/: 28 recaps, 0 already synced, 28 pending\.$/);
    expect(toasts).toEqual([]);
    await act(async () => renderer.unmount());
  });

  test('a check that never reached the list says why on the line', async () => {
    stored = readNow(28, 0);
    toasts.length = 0;
    const renderer = await render();
    check = async () => {
      throw 'A Teams recap sync is already running.';
    };
    await pressCheck(renderer);
    expect(resultLine(renderer)).toBe('Check failed: A Teams recap sync is already running.');
    expect(toasts).toEqual([]);
    await act(async () => renderer.unmount());
  });
});
