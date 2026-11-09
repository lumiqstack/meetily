import { afterAll, beforeEach, describe, expect, mock, test } from 'bun:test';
import { act, create, type ReactTestRenderer } from 'react-test-renderer';

// useDiarizationModel drives the "Identify speakers" controls in the retranscribe
// dialog: it must report the model missing until the download finishes, follow the
// backend's progress events, and surface a failed download as an error.
const originalEvent = { ...await import('@tauri-apps/api/event') };
const originalCore = { ...await import('@tauri-apps/api/core') };
afterAll(() => {
  mock.module('@tauri-apps/api/event', () => originalEvent);
  mock.module('@tauri-apps/api/core', () => originalCore);
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

type Report = { status: string; size_mb: number; path: string; percent?: number };
let statusReport: Report = { status: 'missing', size_mb: 107, path: '/models/x.onnx' };
let downloadOutcome: () => Promise<Report> = async () => statusReport;
const invokeCalls: string[] = [];
mock.module('@tauri-apps/api/core', () => ({
  ...originalCore,
  invoke: async (command: string) => {
    invokeCalls.push(command);
    if (command === 'diarization_model_status') return statusReport;
    if (command === 'diarization_download_model') return downloadOutcome();
    if (command === 'diarization_cancel_download') return undefined;
    throw new Error(`unexpected command ${command}`);
  },
}));

const { useDiarizationModel } = await import('../../src/hooks/useDiarizationModel');

type Snapshot = ReturnType<typeof useDiarizationModel>;
let latest: Snapshot | undefined;
function Probe({ active }: { active: boolean }) {
  latest = useDiarizationModel(active);
  return null;
}

const emit = async (name: string, payload: unknown) => {
  await act(async () => {
    for (const r of registrations.filter(r => r.name === name && r.active)) r.handler({ payload });
  });
};

beforeEach(() => {
  registrations.length = 0;
  invokeCalls.length = 0;
  statusReport = { status: 'missing', size_mb: 107, path: '/models/x.onnx' };
  downloadOutcome = async () => statusReport;
  latest = undefined;
});

describe('useDiarizationModel', () => {
  test('does not query the backend until active, then reports the model missing', async () => {
    let renderer: ReactTestRenderer | undefined;
    await act(async () => {
      renderer = create(<Probe active={false} />);
    });
    expect(invokeCalls).toHaveLength(0);
    expect(latest!.isAvailable).toBe(false);

    await act(async () => renderer!.update(<Probe active />));
    expect(invokeCalls).toEqual(['diarization_model_status']);
    expect(latest!.report?.status).toBe('missing');
    expect(latest!.isAvailable).toBe(false);
    expect(latest!.isDownloading).toBe(false);
    await act(async () => renderer!.unmount());
  });

  test('follows progress events during a download and becomes available on completion', async () => {
    let finish: (() => void) | undefined;
    downloadOutcome = () => new Promise(resolve => {
      finish = () => resolve({ status: 'available', size_mb: 107, path: '/models/x.onnx' });
    });
    await act(async () => {
      create(<Probe active />);
    });

    let downloadPromise: Promise<void> | undefined;
    await act(async () => {
      downloadPromise = latest!.download();
    });
    expect(latest!.isDownloading).toBe(true);
    expect(latest!.downloadPercent).toBe(0);

    await emit('diarization-download-progress', { percent: 37 });
    expect(latest!.downloadPercent).toBe(37);
    expect(latest!.report?.status).toBe('downloading');
    expect(latest!.isAvailable).toBe(false);

    await act(async () => {
      finish!();
      await downloadPromise;
    });
    expect(latest!.isDownloading).toBe(false);
    expect(latest!.isAvailable).toBe(true);
    expect(latest!.error).toBeNull();
  });

  test('a rejected download surfaces its message and re-reads the status', async () => {
    downloadOutcome = async () => { throw 'speaker model download stalled for 60 s'; };
    await act(async () => {
      create(<Probe active />);
    });
    await act(async () => {
      await latest!.download();
    });
    expect(latest!.error).toBe('speaker model download stalled for 60 s');
    expect(latest!.isDownloading).toBe(false);
    expect(latest!.isAvailable).toBe(false);
    expect(invokeCalls.filter(c => c === 'diarization_model_status')).toHaveLength(2);
  });

  test('a failure event from the backend is reported too', async () => {
    await act(async () => {
      create(<Probe active />);
    });
    await emit('diarization-download-failed', { error: 'disk full' });
    expect(latest!.error).toBe('disk full');
    expect(latest!.isDownloading).toBe(false);
  });
});
