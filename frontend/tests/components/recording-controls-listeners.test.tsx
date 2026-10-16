import { afterAll, beforeAll, describe, expect, mock, test } from 'bun:test';
import type React from 'react';
import { act, create, type ReactTestRenderer } from 'react-test-renderer';

// RecordingControls' listeners must survive onRecordingStop changing identity
// (it does at recording start) without leaking: every listener registered must be
// unlistened, even one whose listen() was still pending when the effect cleaned
// up. A leaked `transcript-error` listener calls a stale onRecordingStop again.
const originals = {
  recordingState: null as unknown as Record<string, unknown>,
  event: { ...await import('@tauri-apps/api/event') },
  core: { ...await import('@tauri-apps/api/core') },
  analytics: { ...await import('../../src/lib/analytics') },
};
afterAll(() => {
  if (originals.recordingState) mock.module('../../src/contexts/RecordingStateContext', () => originals.recordingState);
  mock.module('@tauri-apps/api/event', () => originals.event);
  mock.module('@tauri-apps/api/core', () => originals.core);
  mock.module('../../src/lib/analytics', () => originals.analytics);
  if (!hadWindow) delete (globalThis as any).window;
  if (!hadSessionStorage) delete (globalThis as any).sessionStorage;
});
const hadWindow = 'window' in globalThis;
const hadSessionStorage = 'sessionStorage' in globalThis;
(globalThis as any).window ??= { addEventListener() {}, removeEventListener() {} };
(globalThis as any).sessionStorage ??= { getItem: () => null, removeItem() {} };

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
mock.module('@tauri-apps/api/core', () => ({ ...originals.core, invoke: async () => false }));
mock.module('../../src/lib/analytics', () => ({ default: { trackTranscriptionError() {}, trackButtonClick() {} } }));

let RecordingControls: React.FC<any>;
beforeAll(async () => {
  originals.recordingState = { ...await import('../../src/contexts/RecordingStateContext') };
  mock.module('../../src/contexts/RecordingStateContext', () => ({
    ...originals.recordingState,
    useRecordingState: () => ({ isPaused: false, isStartingRecording: false }),
  }));
  ({ RecordingControls } = await import('../../src/components/RecordingControls'));
});

async function resolveAllPending() {
  while (pending.length) {
    await act(async () => {
      for (const r of pending.splice(0)) r.resolve();
    });
  }
}

describe('RecordingControls event listeners', () => {
  test('a re-render while listen() is pending leaks no listener', async () => {
    const props = {
      isRecording: true,
      onRecordingStart: async () => {},
      onTranscriptReceived: () => {},
      isRecordingDisabled: false,
      isParentProcessing: false,
    };
    const firstStop = mock((_callApi?: boolean) => {});
    const secondStop = mock((_callApi?: boolean) => {});
    let renderer: ReactTestRenderer | undefined;
    await act(async () => {
      renderer = create(<RecordingControls {...props} onRecordingStop={firstStop} />);
    });
    await act(async () => renderer!.update(<RecordingControls {...props} onRecordingStop={secondStop} />));
    await resolveAllPending();

    await act(async () => {
      for (const r of registrations.filter(r => r.name === 'transcript-error' && !r.unlistened)) {
        r.handler({ payload: 'boom' });
      }
    });
    expect(firstStop).toHaveBeenCalledTimes(0);
    expect(secondStop.mock.calls).toEqual([[false]]);

    await act(async () => renderer!.unmount());
    await resolveAllPending();
    expect(registrations.length).toBeGreaterThan(0);
    expect(registrations.filter(r => !r.unlistened).map(r => r.name)).toEqual([]);
  });
});
