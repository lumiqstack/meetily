import { describe, expect, test } from "bun:test";

import {
  applySpeakerRename,
  planSpeakerRename,
  renameScopeText,
  renameToast,
} from "../../src/lib/speaker-names";

const SPEAKERS = ["Speaker 1", "Speaker 2", "mic", "Ana"];

describe("planSpeakerRename", () => {
  test("trims and renames to a new name", () => {
    expect(planSpeakerRename("Speaker 1", "  Bruno ", SPEAKERS)).toEqual({
      kind: "rename",
      newSpeaker: "Bruno",
    });
  });

  test("rejects empty and over-long names", () => {
    expect(planSpeakerRename("Speaker 1", "   ", SPEAKERS).kind).toBe("invalid");
    expect(planSpeakerRename("Speaker 1", "x".repeat(81), SPEAKERS).kind).toBe("invalid");
    expect(planSpeakerRename("Speaker 1", "x".repeat(80), SPEAKERS).kind).toBe("rename");
  });

  test("counts characters, not UTF-16 units, against the limit", () => {
    expect(planSpeakerRename("Speaker 1", "😀".repeat(80), SPEAKERS).kind).toBe("rename");
  });

  test("treats the current raw value or display label as unchanged", () => {
    expect(planSpeakerRename("Speaker 1", "Speaker 1", SPEAKERS)).toEqual({ kind: "unchanged" });
    expect(planSpeakerRename("mic", "Me", SPEAKERS)).toEqual({ kind: "unchanged" });
  });

  test("allows a case-only change to the same speaker", () => {
    expect(planSpeakerRename("Ana", "ANA", SPEAKERS)).toEqual({ kind: "rename", newSpeaker: "ANA" });
  });

  test("merges into another speaker, matching case-insensitively", () => {
    expect(planSpeakerRename("Speaker 2", "ana", SPEAKERS)).toEqual({
      kind: "merge",
      newSpeaker: "Ana",
      intoLabel: "Ana",
    });
  });

  test("merging into a source tag targets its raw value", () => {
    expect(planSpeakerRename("Speaker 1", "Me", SPEAKERS)).toEqual({
      kind: "merge",
      newSpeaker: "mic",
      intoLabel: "Me",
    });
  });
});

describe("renameToast", () => {
  test("names both labels and the count", () => {
    const toast = renameToast("Speaker 1", { kind: "rename", newSpeaker: "Ana" }, 42, false);
    expect(toast).toEqual({ title: "Renamed Speaker 1 to Ana in 42 segments" });
  });

  test("uses display labels and singular count", () => {
    const toast = renameToast("mic", { kind: "rename", newSpeaker: "Marcos" }, 1, false);
    expect(toast.title).toBe("Renamed Me to Marcos in 1 segment");
  });

  test("says merged for a merge", () => {
    const toast = renameToast("Speaker 2", { kind: "merge", newSpeaker: "Ana", intoLabel: "Ana" }, 7, false);
    expect(toast.title).toBe("Merged Speaker 2 into Ana (7 segments)");
  });

  test("warns that an existing summary keeps the old label", () => {
    const toast = renameToast("Speaker 1", { kind: "rename", newSpeaker: "Ana" }, 3, true);
    expect(toast.description).toContain('"Speaker 1"');
    expect(toast.description).toContain("regenerate");
  });
});

describe("renameScopeText", () => {
  test("pluralizes", () => {
    expect(renameScopeText(42)).toBe("Applies to all 42 segments by this speaker in this meeting");
    expect(renameScopeText(1)).toBe("Applies to the 1 segment by this speaker in this meeting");
  });
});

describe("applySpeakerRename", () => {
  test("rewrites only exact raw matches", () => {
    const items = [
      { id: "a", speaker: "Speaker 1" },
      { id: "b", speaker: "Speaker 10" },
      { id: "c", speaker: null },
    ];
    expect(applySpeakerRename(items, "Speaker 1", "Ana")).toEqual([
      { id: "a", speaker: "Ana" },
      { id: "b", speaker: "Speaker 10" },
      { id: "c", speaker: null },
    ]);
  });

  test("returns the same array when nothing matches", () => {
    const items = [{ id: "a", speaker: "Ana" }];
    expect(applySpeakerRename(items, "Speaker 1", "Bruno")).toBe(items);
  });
});
