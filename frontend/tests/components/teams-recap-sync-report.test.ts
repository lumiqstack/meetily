import { describe, expect, test } from 'bun:test';
import { describeLastListing, describeSyncReport, type LastListingView } from '../../src/components/TeamsRecapSyncSettings';

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

describe('describeLastListing', () => {
  // Read at 12:40 PM in Mexico City; the window starts on Oct 28 there.
  const listing: LastListingView = {
    readAt: '2026-11-06T18:40:20+00:00', windowStart: '2026-10-29T01:16:21+00:00',
    listed: 28, synced: 0, pending: 28, notReady: 0, error: null, endedEarlyAt: null,
  };
  const options = { now: new Date('2026-11-06T19:05:00Z'), locale: 'en-US', timeZone: 'America/Mexico_City' };
  // ICU may put a narrow no-break space before AM/PM.
  const line = (last: LastListingView, opts = options) => describeLastListing(last, opts).replace(/ /g, ' ');

  test('says when the list was read and how its recaps stand', () => {
    expect(line(listing)).toBe('Checked at 12:40 PM: 28 recaps, 0 already synced, 28 pending.');
    expect(line({ ...listing, listed: 1, synced: 1, pending: 0, notReady: 2 })).toBe(
      'Checked at 12:40 PM: 1 recap, 1 already synced, 0 pending, 2 without a recap yet.'
    );
  });

  test('names the day when the list was read on another one', () => {
    expect(line(listing, { ...options, now: new Date('2026-11-07T15:00:00Z') })).toBe(
      'Checked Nov 6 at 12:40 PM: 28 recaps, 0 already synced, 28 pending.'
    );
  });

  test('warns when the list stopped before the window start', () => {
    expect(line({ ...listing, listed: 7, pending: 7, endedEarlyAt: '2026-11-05T15:00:00.000Z' })).toBe(
      'Checked at 12:40 PM: 7 recaps, 0 already synced, 7 pending. The list stopped at Nov 5, before Oct 28, so it may be missing older recaps. Check again.'
    );
  });

  test('says so when the window holds no meetings', () => {
    expect(line({ ...listing, listed: 0, pending: 0 })).toBe('Checked at 12:40 PM: no recaps since Oct 28.');
  });

  test('gives the reason when the list was not read', () => {
    expect(line({ ...listing, listed: 0, pending: 0, error: 'Teams did not show the Meeting Recaps list.' })).toBe(
      'Checked at 12:40 PM: the list was not read. Teams did not show the Meeting Recaps list.'
    );
  });
});
