import { afterAll, describe, expect, mock, test } from 'bun:test';
import { act, create, type ReactTestRenderer } from 'react-test-renderer';

// When the configured data root (an external or network drive) is not mounted,
// the app must say so instead of showing onboarding or an empty meetings list.
const originalCore = { ...await import('@tauri-apps/api/core') };
afterAll(() => {
  mock.module('@tauri-apps/api/core', () => originalCore);
});

let unavailableRoot: string | null = null;
const invoke = mock(async (command: string): Promise<unknown> => {
  if (command === 'get_unavailable_data_root') return unavailableRoot;
  return undefined;
});
mock.module('@tauri-apps/api/core', () => ({ ...originalCore, invoke }));
const { DataRootUnavailableGate } = await import('../../src/components/DataRootUnavailableGate');

async function render(): Promise<ReactTestRenderer> {
  let renderer: ReactTestRenderer | undefined;
  await act(async () => {
    renderer = create(
      <DataRootUnavailableGate>
        <div id="app">app</div>
      </DataRootUnavailableGate>,
    );
  });
  return renderer!;
}

describe('DataRootUnavailableGate', () => {
  test('blocks the app with the unavailable folder when the data root is unreachable', async () => {
    unavailableRoot = 'E:\\Meetily';
    const renderer = await render();
    const text = JSON.stringify(renderer.toJSON());
    expect(text).toContain('E:\\\\Meetily');
    expect(text).toContain('restart');
    expect(renderer.root.findAll((n) => n.props.id === 'app')).toHaveLength(0);
    await act(async () => renderer.unmount());
  });

  test('renders the app when the data root is available', async () => {
    unavailableRoot = null;
    const renderer = await render();
    expect(renderer.root.findAll((n) => n.props.id === 'app')).toHaveLength(1);
    await act(async () => renderer.unmount());
  });
});
