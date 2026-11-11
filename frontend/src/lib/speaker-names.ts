import { displaySpeaker } from "@/lib/speaker-label";

/** One row of `api_get_meeting_speakers`: a distinct stored speaker value. */
export interface MeetingSpeaker {
  /** The raw stored value ("mic", "Speaker 1", "Jane Smith"), not the display label. */
  speaker: string;
  segment_count: number;
  first_start: number | null;
}

/** The Rust core rejects longer names (`api_rename_meeting_speaker`). */
export const MAX_SPEAKER_NAME_LENGTH = 80;

/**
 * What saving a typed name for `oldSpeaker` would do.
 * - `merge`: the name matches another speaker of the meeting, so both become
 *   one; `newSpeaker` is that speaker's raw value so the stored values match.
 */
export type SpeakerRenamePlan =
  | { kind: "invalid"; reason: string }
  | { kind: "unchanged" }
  | { kind: "rename"; newSpeaker: string }
  | { kind: "merge"; newSpeaker: string; intoLabel: string };

function labelOf(speaker: string): string {
  return displaySpeaker(speaker) ?? speaker;
}

export function planSpeakerRename(
  oldSpeaker: string,
  input: string,
  speakers: readonly string[],
): SpeakerRenamePlan {
  const name = input.trim();
  if (name === "") return { kind: "invalid", reason: "Enter a name." };
  if (Array.from(name).length > MAX_SPEAKER_NAME_LENGTH) {
    return {
      kind: "invalid",
      reason: `Use at most ${MAX_SPEAKER_NAME_LENGTH} characters.`,
    };
  }
  if (name === oldSpeaker || name === labelOf(oldSpeaker)) {
    return { kind: "unchanged" };
  }

  const folded = name.toLocaleLowerCase();
  const match = speakers.find(
    (other) =>
      other !== oldSpeaker &&
      (other.toLocaleLowerCase() === folded ||
        labelOf(other).toLocaleLowerCase() === folded),
  );
  if (match) {
    return { kind: "merge", newSpeaker: match, intoLabel: labelOf(match) };
  }
  return { kind: "rename", newSpeaker: name };
}

function segments(count: number): string {
  return count === 1 ? "1 segment" : `${count} segments`;
}

export function renameScopeText(segmentCount: number): string {
  return segmentCount === 1
    ? "Applies to the 1 segment by this speaker in this meeting"
    : `Applies to all ${segmentCount} segments by this speaker in this meeting`;
}

export function renameToast(
  oldSpeaker: string,
  plan: Extract<SpeakerRenamePlan, { kind: "rename" | "merge" }>,
  changed: number,
  hasSummary: boolean,
): { title: string; description?: string } {
  const from = labelOf(oldSpeaker);
  const title =
    plan.kind === "merge"
      ? `Merged ${from} into ${plan.intoLabel} (${segments(changed)})`
      : `Renamed ${from} to ${labelOf(plan.newSpeaker)} in ${segments(changed)}`;
  return hasSummary
    ? {
        title,
        description: `The existing summary still says "${from}" until you regenerate it.`,
      }
    : { title };
}

/**
 * Mirrors the backend rewrite on segments already loaded in memory. Returns
 * the same array when nothing matched, so React state stays referentially equal.
 */
export function applySpeakerRename<T extends { speaker?: string | null }>(
  items: T[],
  oldSpeaker: string,
  newSpeaker: string,
): T[] {
  if (!items.some((item) => item.speaker === oldSpeaker)) return items;
  return items.map((item) =>
    item.speaker === oldSpeaker ? { ...item, speaker: newSpeaker } : item,
  );
}
