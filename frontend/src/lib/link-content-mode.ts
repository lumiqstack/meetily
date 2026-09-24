// Recognize Teams recap deep links first, then guess recording/transcript
// links from the `id` filename. Returns null when it cannot tell.
export function detectLinkContentMode(url: string): 'audio' | 'transcript' | 'recap' | null {
  let raw = url.toLowerCase();
  try {
    const u = new URL(url);
    if (/^teams\.(microsoft\.com|cloud\.microsoft)(\.mcas\.ms)?$/i.test(u.hostname)
      && u.pathname === '/l/meetingrecap') {
      return 'recap';
    }
    raw = (u.searchParams.get('id') || u.pathname).toLowerCase();
  } catch {
    // fall back to scanning the whole string
  }
  if (raw.includes('transcript')) return 'transcript';
  if (raw.includes('recording')) return 'audio';
  return null;
}
