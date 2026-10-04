/** Teams' recording stamp in a file name: `…-20260721_100221-Meeting Recording`. */
const TEAMS_STAMP = /-(\d{4})(\d{2})(\d{2})_(\d{2})(\d{2})(\d{2})-Meeting (?:Recording|Transcript)/i;

/**
 * Sort key for when a recorded meeting took place, matching the backend's
 * meeting date: the Teams stamp (local start time) when the name has one,
 * otherwise SharePoint's file creation time. Unknown dates sort last.
 */
function meetingTime(rec: { name: string; created: string }): number {
  const stamp = TEAMS_STAMP.exec(rec.name);
  if (stamp) {
    const [, y, mo, d, h, mi, s] = stamp.map(Number);
    return new Date(y, mo - 1, d, h, mi, s).getTime();
  }
  const created = Date.parse(rec.created);
  return Number.isNaN(created) ? Number.POSITIVE_INFINITY : created;
}

/** Oldest meeting first, so imported meetings are processed in the order they happened. */
export function oldestMeetingFirst<T extends { name: string; created: string }>(recordings: T[]): T[] {
  return [...recordings].sort((a, b) => meetingTime(a) - meetingTime(b));
}
