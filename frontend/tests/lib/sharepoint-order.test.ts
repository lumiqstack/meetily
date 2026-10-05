import { describe, expect, test } from 'bun:test';

import { oldestMeetingFirst } from '../../src/lib/sharepoint-order';

const rec = (name: string, created: string) => ({ name, created });

describe('oldestMeetingFirst', () => {
  test('orders a newest-first scan from the oldest meeting to the newest', () => {
    const scan = [
      rec('Retro-20261002_160000-Meeting Recording.mp4', '2026-10-02T17:01:00Z'),
      rec('Kickoff-20260930_083000-Meeting Recording.mp4', '2026-09-30T15:31:00Z'),
      rec('Weekly sync-20260721_100221-Meeting Recording.mp4', '2026-07-21T15:48:00Z'),
    ];
    expect(oldestMeetingFirst(scan).map((r) => r.name.split('-')[0])).toEqual(['Weekly sync', 'Kickoff', 'Retro']);
  });

  test('prefers the Teams stamp over a later upload time', () => {
    // Uploaded/re-saved later than a meeting that actually happened after it.
    const early = rec('Planning-20260901_090000-Meeting Recording.mp4', '2026-09-20T10:00:00Z');
    const late = rec('Review-20260910_090000-Meeting Recording.mp4', '2026-09-10T10:00:00Z');
    expect(oldestMeetingFirst([late, early])).toEqual([early, late]);
  });

  test('falls back to the creation time and puts unknown dates last without mutating input', () => {
    const scan = [
      rec('audio.mp4', ''),
      rec('Town hall.mp4', '2026-09-20T17:45:00Z'),
      rec('Quarterly review.mp4', '2026-09-15T15:09:30Z'),
    ];
    const sorted = oldestMeetingFirst(scan);
    expect(sorted.map((r) => r.name)).toEqual(['Quarterly review.mp4', 'Town hall.mp4', 'audio.mp4']);
    expect(scan[0].name).toBe('audio.mp4');
  });
});
