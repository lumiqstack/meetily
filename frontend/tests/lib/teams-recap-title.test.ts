import { expect, test } from 'bun:test';
import { recapTitleFromFileUrl } from '../../src/lib/teams-recap-title';

test('cleans recording and transcript suffixes while keeping the meeting name', () => {
  for (const kind of ['Recording', 'Transcript']) {
    expect(recapTitleFromFileUrl(`https://tenant.sharepoint.com/Monthly%20Review-20260924_140734-Meeting%20${kind}.mp4?web=1`)).toBe('Monthly Review');
  }
});
test('preserves an ordinary title without a Teams suffix', () => {
  expect(recapTitleFromFileUrl('https://tenant.sharepoint.com/2026%20Planning.mp4')).toBe('2026 Planning');
});
test('invalid links do not crash title suggestions', () => {
  expect(recapTitleFromFileUrl('not a URL')).toBeNull();
});
