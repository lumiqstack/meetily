import { describe, expect, test } from 'bun:test';
import { describeSyncReport } from '../../src/components/TeamsRecapSyncSettings';

const base = {
  scanned: 0, new: 0, updated: 0, skipped: 0, notReady: 0, failed: [], possibleDuplicates: [], meetingsFolder: 'V/Meetings',
};

describe('describeSyncReport', () => {
  test('says so when the window listed nothing', () => {
    expect(describeSyncReport(base)).toBe('No recaps listed in the scan window.');
  });

  test('lists the three steady counts and only the troubles that occurred', () => {
    expect(describeSyncReport({ ...base, scanned: 6, new: 4, updated: 1, skipped: 1 })).toBe(
      '6 scanned: 4 new, 1 updated, 1 unchanged.'
    );
    expect(
      describeSyncReport({
        ...base, scanned: 3, skipped: 1, notReady: 1,
        failed: [{ title: 'Standup', error: 'boom' }],
      })
    ).toBe('3 scanned: 0 new, 0 updated, 1 unchanged, 1 not ready yet, 1 failed.');
  });

  test('names a recap that may duplicate a live recording', () => {
    expect(describeSyncReport({ ...base, scanned: 2, new: 2, possibleDuplicates: ['Vendor call'] })).toBe(
      '2 scanned: 2 new, 0 updated, 0 unchanged. "Vendor call" may duplicate a live recording: saved separately because nothing confirmed the match.'
    );
    expect(describeSyncReport({ ...base, scanned: 3, new: 3, possibleDuplicates: ['Vendor call', 'Standup', 'Review'] })).toBe(
      '3 scanned: 3 new, 0 updated, 0 unchanged. "Vendor call" and 2 more may duplicate a live recording: saved separately because nothing confirmed the match.'
    );
  });
});
