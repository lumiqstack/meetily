import { afterAll, describe, expect, mock, test } from 'bun:test';
import { act, create, type ReactTestRenderer } from 'react-test-renderer';

// useModalState subscribes to `model-download-complete` to auto-close the model
// selector. A config change must replace that subscription, not add another, or
// one download shows one "Model ready!" toast per past config.
const originalEvent = { ...await import('@tauri-apps/api/event') };
const originalSonner = { ...await import('sonner') };
afterAll(() => {
  mock.module('@tauri-apps/api/event', () => originalEvent);
  mock.module('sonner', () => originalSonner);
});

type Registration = { name: string; handler: (event: { payload: unknown }) => void; active: boolean };
const registrations: Registration[] = [];
mock.module('@tauri-apps/api/event', () => ({
  ...originalEvent,
  listen: async (name: string, handler: Registration['handler']) => {
    const registration = { name, handler, active: true };
    registrations.push(registration);
    return () => { registration.active = false; };
  },
}));
const toastSuccess = mock((_message: string, _options?: unknown) => {});
mock.module('sonner', () => ({ toast: { success: toastSuccess, error() {}, info() {}, warning() {} } }));
const { useModalState } = await import('../../src/hooks/useModalState');

const active = (name: string) => registrations.filter(r => r.name === name && r.active);

describe('model-download-complete listener', () => {
  test('config changes keep a single listener, and unmount removes it', async () => {
    function Probe({ config }: { config: { provider: 'localWhisper'; model: string } }) {
      useModalState(config as any);
      return null;
    }
    let renderer: ReactTestRenderer | undefined;
    await act(async () => {
      renderer = create(<Probe config={{ provider: 'localWhisper', model: 'base' }} />);
    });
    await act(async () => renderer!.update(<Probe config={{ provider: 'localWhisper', model: 'base' }} />));
    await act(async () => renderer!.update(<Probe config={{ provider: 'localWhisper', model: 'base' }} />));

    expect(active('model-download-complete')).toHaveLength(1);
    await act(async () => {
      for (const r of active('model-download-complete')) r.handler({ payload: { modelName: 'base' } });
    });
    expect(toastSuccess).toHaveBeenCalledTimes(1);

    await act(async () => renderer!.unmount());
    expect(registrations.filter(r => r.active)).toHaveLength(0);
  });
});
