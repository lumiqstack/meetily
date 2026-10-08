import { afterAll, describe, expect, mock, test } from 'bun:test';
import { useEffect } from 'react';
import { act, create, type ReactTestRenderer } from 'react-test-renderer';

// H7-F6: the desync poll must run under Tauri 2 globals.
// src-tauri/tauri.conf.json does not set withGlobalTauri, so a Tauri 2 webview
// exposes window.__TAURI_INTERNALS__ instead and the desync poll never runs.
const originalRecordingService = { ...await import('../../src/services/recordingService') };
const hadWindow = 'window' in globalThis;
afterAll(() => {
  mock.module('../../src/services/recordingService', () => originalRecordingService);
  if (!hadWindow) delete (globalThis as any).window;
  delete (globalThis as any).__TAURI_INTERNALS__;
});

const isRecording = mock(async () => true);
mock.module('../../src/services/recordingService', () => ({ recordingService: { isRecording } }));
const { useRecordingStateSync } = await import('../../src/hooks/useRecordingStateSync');

describe('recording state sync under Tauri 2 globals', () => {
  test('backend recording is synced into UI when only __TAURI_INTERNALS__ is present', async () => {
    (globalThis as any).window ??= globalThis;
    (globalThis as any).__TAURI_INTERNALS__ = {};
    const setIsRecording = mock((_value: boolean) => {});
    const setIsMeetingActive = mock((_value: boolean) => {});
    function Probe() {
      useEffect(() => {}, []);
      useRecordingStateSync(false, setIsRecording, setIsMeetingActive);
      return null;
    }
    let renderer: ReactTestRenderer | undefined;
    await act(async () => {
      renderer = create(<Probe />);
    });
    await act(async () => renderer!.unmount());

    expect(setIsRecording.mock.calls).toEqual([[true]]);
    expect(setIsMeetingActive.mock.calls).toEqual([[true]]);
  });
});
