/**
 * Stored speaker values for live recordings are source tags ("mic" /
 * "system" — see migration 20251110000001); imported transcripts (Teams VTT)
 * store real participant names. This maps tags to their display labels and
 * passes names through, so every place that renders or serializes a speaker
 * shows "Me: …" instead of "mic: …".
 */
const SOURCE_LABELS: Record<string, string> = {
  mic: "Me",
  system: "Others",
};

export function displaySpeaker(
  speaker: string | null | undefined,
): string | null {
  if (!speaker) return null;
  return SOURCE_LABELS[speaker] ?? speaker;
}
