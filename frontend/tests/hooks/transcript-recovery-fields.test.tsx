import { afterAll, describe, expect, mock, test } from 'bun:test';
import { act, create } from 'react-test-renderer';

// Recovered transcripts must keep `speaker` and order by the stored
// `sequence_id`: rows are written by TranscriptContext as the raw TranscriptUpdate.
const originals = {
  core: { ...await import('@tauri-apps/api/core') },
  idb: { ...await import('../../src/services/indexedDBService') },
  storage: { ...await import('../../src/services/storageService') },
  prefs: { ...await import('../../src/lib/summary-language-preferences') },
  jobs: { ...await import('../../src/components/shared/BackgroundJobToast') },
  config: { ...await import('../../src/contexts/ConfigContext') },
  sonner: { ...await import('sonner') },
};
afterAll(() => {
  mock.module('@tauri-apps/api/core', () => originals.core);
  mock.module('../../src/services/indexedDBService', () => originals.idb);
  mock.module('../../src/services/storageService', () => originals.storage);
  mock.module('../../src/lib/summary-language-preferences', () => originals.prefs);
  mock.module('../../src/components/shared/BackgroundJobToast', () => originals.jobs);
  mock.module('../../src/contexts/ConfigContext', () => originals.config);
  mock.module('sonner', () => originals.sonner);
});

const storedRows: any[] = [];
const indexedDBService = {
  getMeetingMetadata: async () => ({
    meetingId: 'meeting-a', title: 'Recovered call', startTime: 1, lastUpdated: 1,
    transcriptCount: storedRows.length, savedToSQLite: false,
  }),
  getTranscripts: async () => storedRows,
  markMeetingSaved: async () => {},
};
mock.module('../../src/services/indexedDBService', () => ({ indexedDBService }));
const saveMeeting = mock(async (_title: string, _rows: unknown[], _folder: unknown) => ({ meeting_id: 'saved-a' }));
mock.module('../../src/services/storageService', () => ({ storageService: { saveMeeting } }));
mock.module('@tauri-apps/api/core', () => ({
  invoke: async () => { throw new Error('no folder'); },
}));
mock.module('../../src/lib/summary-language-preferences', () => ({
  applyPinnedSummaryLanguageToMeeting: async () => {},
  detectAndCacheSummaryLanguage: async () => ({ language: 'en' }),
}));
mock.module('../../src/components/shared/BackgroundJobToast', () => ({ backgroundJobStore: { remove() {} } }));
mock.module('../../src/contexts/ConfigContext', () => ({
  useConfig: () => ({ transcriptModelConfig: { provider: 'whisper', model: 'base' }, selectedLanguage: 'auto' }),
}));
mock.module('sonner', () => ({ toast: { warning() {}, success() {}, error() {}, info() {} } }));
const { useTranscriptRecovery } = await import('../../src/hooks/useTranscriptRecovery');

let current: ReturnType<typeof useTranscriptRecovery>;
function Probe() {
  current = useTranscriptRecovery();
  return null;
}

async function recoverStoredRows(rows: any[]) {
  storedRows.length = 0;
  storedRows.push(...rows);
  saveMeeting.mockClear();

  await act(async () => {
    create(<Probe />);
  });
  await act(async () => {
    await current.recoverMeeting('meeting-a');
  });

  expect(saveMeeting.mock.calls.length).toBe(1);
  return saveMeeting.mock.calls[0][1] as any[];
}

// Shape written by TranscriptContext.saveTranscript: the spread TranscriptUpdate.
function storedRow(sequence_id: number, speaker: string) {
  return {
    id: sequence_id, meetingId: 'meeting-a', text: 'hi', timestamp: 'x', confidence: 0.9, storedAt: 1,
    sequence_id, speaker,
  };
}

describe('recovered transcripts keep speaker and sequence_id', () => {
  test('recovery keeps the stored speaker', async () => {
    const rows = await recoverStoredRows([storedRow(3, 'mic')]);
    expect(rows.length).toBe(1);
    expect(rows[0].speaker).toBe('mic');
  });

  test('recovery keeps the stored sequence_id', async () => {
    const rows = await recoverStoredRows([storedRow(3, 'mic')]);
    expect(rows.length).toBe(1);
    expect(rows[0].sequence_id).toBe(3);
  });

  test('recovery saves rows in sequence_id order', async () => {
    const rows = await recoverStoredRows([storedRow(5, 'system'), storedRow(2, 'mic')]);
    expect(rows.map(r => r.sequence_id)).toEqual([2, 5]);
  });
});
