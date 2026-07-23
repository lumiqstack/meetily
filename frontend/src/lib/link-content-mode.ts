// Guess whether a link points at a recording or a Teams transcript, from the
// file name in the `id` param. Returns null when it can't tell.
export function detectLinkContentMode(url: string): 'audio' | 'transcript' | null {
  let raw = url.toLowerCase();
  try {
    const u = new URL(url);
    raw = (u.searchParams.get('id') || u.pathname).toLowerCase();
  } catch {
    // fall back to scanning the whole string
  }
  if (raw.includes('transcript')) return 'transcript';
  if (raw.includes('recording')) return 'audio';
  return null;
}
