import { describe, expect, test } from 'bun:test';
import {
  deleteReportText,
  formatBytes,
  purgeConfirmText,
  purgeResultText,
  selectOlderThan,
  summarizeSelection,
  toggle,
  type Survey,
} from '../../src/lib/recordings';

const held = { code: 'not_transcribed', text: 'Not transcribed yet.' } as const;
const now = new Date('2026-11-23T12:00:00Z');

const survey: Survey = {
  meetings: [
    { meeting_id: 'old', title: 'Old', created_at: '2026-01-10T10:00:00Z', audio: { state: 'removable', bytes: 1000 } },
    { meeting_id: 'old-held', title: 'Old held', created_at: '2026-01-11T10:00:00Z', audio: { state: 'held', bytes: 500, reason: held } },
    { meeting_id: 'recent', title: 'Recent', created_at: '2026-11-20T10:00:00Z', audio: { state: 'removable', bytes: 300 } },
  ],
  orphans: [
    { folder: '/rec/Orphan', name: 'Orphan', modified_at: '2026-01-01T00:00:00Z', bytes: 200, hold: null },
    { folder: '/rec/Fresh', name: 'Fresh', modified_at: '2026-11-22T00:00:00Z', bytes: 50, hold: { code: 'recently_active', text: 'Changed recently.' } },
  ],
  volumes: [],
  audio_bytes: 2050,
};

describe('recordings selection', () => {
  test('selectOlderThan picks only removable meetings past the cutoff and never orphans', () => {
    expect(selectOlderThan(survey, 30, now)).toEqual({ meetingIds: ['old'], orphanFolders: [] });
    expect(selectOlderThan(survey, 1, now).meetingIds).toEqual(['old', 'recent']);
    expect(selectOlderThan(survey, 365, now).meetingIds).toEqual([]);
  });

  test('summarizeSelection ignores held, gone and unknown targets', () => {
    expect(
      summarizeSelection(survey, {
        meetingIds: ['old', 'old-held', 'vanished'],
        orphanFolders: ['/rec/Orphan', '/rec/Fresh', '/elsewhere'],
      }),
    ).toEqual({ meetings: 1, orphans: 1, bytes: 1200 });
    expect(summarizeSelection(survey, { meetingIds: [], orphanFolders: [] })).toEqual({ meetings: 0, orphans: 0, bytes: 0 });
  });

  test('toggle adds once and removes', () => {
    expect(toggle(['a'], 'a', true)).toEqual(['a']);
    expect(toggle(['a'], 'b', true)).toEqual(['a', 'b']);
    expect(toggle(['a', 'b'], 'a', false)).toEqual(['b']);
  });
});

describe('recordings text', () => {
  test('formatBytes', () => {
    expect(formatBytes(512)).toBe('512 B');
    expect(formatBytes(1536)).toBe('1.5 KB');
    expect(formatBytes(48 * 1024 * 1024)).toBe('48.0 MB');
    expect(formatBytes(1.3 * 1024 * 1024 * 1024)).toBe('1.3 GB');
  });

  test('confirm text states the count and the bytes', () => {
    expect(purgeConfirmText({ meetings: 14, orphans: 2, bytes: 1.3 * 1024 ** 3 })).toBe(
      'Permanently remove the audio of 14 meetings and 2 leftover folders? This frees 1.3 GB.',
    );
    expect(purgeConfirmText({ meetings: 1, orphans: 0, bytes: 2048 })).toBe(
      'Permanently remove the audio of 1 meeting? This frees 2.0 KB.',
    );
  });

  test('result text reports what was kept and what failed', () => {
    const plain = purgeResultText({ freed_bytes: 1024, purged_meetings: ['a'], purged_orphans: 0, skipped: [], failed: [] });
    expect(plain).toEqual({ title: 'Freed 1.0 KB', description: undefined });
    const mixed = purgeResultText({
      freed_bytes: 0,
      purged_meetings: [],
      purged_orphans: 0,
      skipped: [{ target: 'a', reason: held }],
      failed: [{ target: 'b', files: ['audio.mp4 (PermissionDenied)'] }],
    });
    expect(mixed.description).toContain('1 item was kept: Not transcribed yet.');
    expect(mixed.description).toContain('1 item could not be fully removed');
  });

  test('every delete report has its own sentence', () => {
    const reports = [
      { folder: 'none' },
      { folder: 'removed', freed_bytes: 2048 },
      { folder: 'kept_busy' },
      { folder: 'kept_shared' },
      { folder: 'kept_unrecognized' },
      { folder: 'kept_foreign_files', freed_bytes: 10, path: '/rec/x' },
      { folder: 'failed', path: '/rec/y' },
    ] as const;
    const texts = reports.map((r) => deleteReportText(r));
    expect(new Set(texts).size).toBe(reports.length);
    expect(texts[1]).toBe('Recording folder removed, 2.0 KB freed.');
    expect(texts[2]).toContain('Settings, Storage lists it');
  });
});
