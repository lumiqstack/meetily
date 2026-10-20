import { afterAll, beforeAll, describe, expect, mock, test } from 'bun:test';
import type React from 'react';
import { act, create, type ReactTestRenderer } from 'react-test-renderer';

// The Whisper model manager's download listeners must not outlive it when it
// unmounts (settings closed) while listen() is still pending. A leaked
// `model-download-complete` listener repeats the "ready!" toast and the
// auto-select/save for every leaked instance.
const originals = {
  event: { ...await import('@tauri-apps/api/event') },
  core: { ...await import('@tauri-apps/api/core') },
  whisper: { ...await import('../../src/lib/whisper') },
  sonner: { ...await import('sonner') },
};
const hadLocalStorage = 'localStorage' in globalThis;
afterAll(() => {
  mock.module('@tauri-apps/api/event', () => originals.event);
  mock.module('@tauri-apps/api/core', () => originals.core);
  mock.module('../../src/lib/whisper', () => originals.whisper);
  mock.module('sonner', () => originals.sonner);
  if (!hadLocalStorage) delete (globalThis as any).localStorage;
});
(globalThis as any).localStorage ??= { getItem: () => null, setItem() {}, removeItem() {} };

type Registration = {
  name: string;
  handler: (event: { payload: unknown }) => void;
  unlistened: boolean;
  resolve: () => void;
};
const registrations: Registration[] = [];
const pending: Registration[] = [];
mock.module('@tauri-apps/api/event', () => ({
  ...originals.event,
  listen: (name: string, handler: Registration['handler']) =>
    new Promise<() => void>(resolveListen => {
      const registration: Registration = {
        name,
        handler,
        unlistened: false,
        resolve: () => resolveListen(() => { registration.unlistened = true; }),
      };
      registrations.push(registration);
      pending.push(registration);
    }),
}));
mock.module('@tauri-apps/api/core', () => ({ ...originals.core, invoke: async () => undefined }));
// Model loading never finishes; the component stays on its loading skeleton.
mock.module('../../src/lib/whisper', () => ({
  ...originals.whisper,
  WhisperAPI: { init: () => new Promise(() => {}), getAvailableModels: async () => [] },
}));
const success = mock((_message: string, _options?: unknown) => {});
mock.module('sonner', () => ({ toast: { success, error() {}, info() {}, warning() {} } }));

let ModelManager: React.FC<any>;
beforeAll(async () => {
  ({ ModelManager } = await import('../../src/components/WhisperModelManager'));
});

async function resolveAllPending() {
  while (pending.length) {
    await act(async () => {
      for (const r of pending.splice(0)) r.resolve();
    });
  }
}

describe('WhisperModelManager download listeners', () => {
  test('an unmount while listen() is pending leaks no listener', async () => {
    const onModelSelect = mock((_name: string) => {});
    let first: ReactTestRenderer | undefined;
    await act(async () => { first = create(<ModelManager onModelSelect={onModelSelect} />); });
    await act(async () => first!.unmount());
    await resolveAllPending();
    expect(registrations.length).toBeGreaterThan(0);
    expect(registrations.filter(r => !r.unlistened).map(r => r.name)).toEqual([]);

    let second: ReactTestRenderer | undefined;
    await act(async () => { second = create(<ModelManager onModelSelect={onModelSelect} />); });
    await resolveAllPending();
    await act(async () => {
      for (const r of registrations.filter(r => r.name === 'model-download-complete' && !r.unlistened)) {
        r.handler({ payload: { modelName: 'base' } });
      }
    });
    expect(onModelSelect.mock.calls).toEqual([['base']]);
    expect(success).toHaveBeenCalledTimes(1);

    await act(async () => second!.unmount());
    await resolveAllPending();
    expect(registrations.filter(r => !r.unlistened).map(r => r.name)).toEqual([]);
  });
});
