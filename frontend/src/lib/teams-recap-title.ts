/** Remove Teams' recording/transcript suffix from the suggested meeting title. */
export function recapTitleFromFileUrl(fileUrl: string): string | null {
  try {
    const fileName = new URL(fileUrl).pathname.split('/').filter(Boolean).pop() || '';
    const title = decodeURIComponent(fileName)
      .replace(/\.(mp4|vtt)$/i, '')
      .replace(/-Meeting (?:Transcript|Recording)$/i, '')
      .replace(/-\d{8}_\d{6}$/, '')
      .trim();
    return title || null;
  } catch {
    return null;
  }
}
