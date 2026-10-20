import { afterAll, beforeAll, describe, expect, mock, test } from 'bun:test';
import type React from 'react';
import { act, create, type ReactTestRenderer } from 'react-test-renderer';

// RecordingStateProvider registers its listeners with a chain of awaited
// listen() calls. Unmounting while one is pending must still unlisten every
// listener that resolves afterwards.
const originals = {
  event: { ...await import('@tauri-apps/api/event') },
  core: { ...await import('@tauri-apps/api/core') },
};
afterAll(() => {
  mock.module('@tauri-apps/api/event', () => originals.event);
  mock.module('@tauri-apps/api/core', () => originals.core);
});

type Registration = { name: string; unlistened: boolean; resolve: () => void };
const registrations: Registration[] = [];
const pending: Registration[] = [];
mock.module('@tauri-apps/api/event', () => ({
  ...originals.event,
  listen: (name: string) =>
    new Promise<() => void>(resolveListen => {
      const registration: Registration = {
        name,
        unlistened: false,
        resolve: () => resolveListen(() => { registration.unlistened = true; }),
      };
      registrations.push(registration);
      pending.push(registration);
    }),
}));
mock.module('@tauri-apps/api/core', () => ({
  ...originals.core,
  invoke: async () => ({
    is_recording: false, is_paused: false, is_active: false,
    recording_duration: null, active_duration: null,
  }),
}));

let RecordingStateProvider: React.FC<{ children: React.ReactNode }>;
beforeAll(async () => {
  ({ RecordingStateProvider } = await import('../../src/contexts/RecordingStateContext'));
});

async function resolveAllPending() {
  while (pending.length) {
    await act(async () => {
      for (const r of pending.splice(0)) r.resolve();
    });
  }
}

describe('RecordingStateProvider event listeners', () => {
  test('unmounting while listen() is pending leaks no listener', async () => {
    let renderer: ReactTestRenderer | undefined;
    await act(async () => {
      renderer = create(<RecordingStateProvider>{null}</RecordingStateProvider>);
    });
    await act(async () => renderer!.unmount());
    await resolveAllPending();

    expect(registrations.map(r => r.name)).toContain('recording-started');
    expect(registrations.filter(r => !r.unlistened).map(r => r.name)).toEqual([]);
  });
});
