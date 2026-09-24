import { describe, expect, it } from "bun:test";

import { isRequestStale } from "../../src/hooks/usePaginatedTranscripts";

describe("usePaginatedTranscripts stale-request guard", () => {
  it("treats a response from an older meeting as stale after navigation", () => {
    expect(
      isRequestStale({
        activeMeetingId: "meeting-b",
        requestMeetingId: "meeting-a",
        activeGeneration: 4,
        requestGeneration: 3,
      }),
    ).toBe(true);
  });

  it("keeps the same meeting and generation valid", () => {
    expect(
      isRequestStale({
        activeMeetingId: "meeting-a",
        requestMeetingId: "meeting-a",
        activeGeneration: 3,
        requestGeneration: 3,
      }),
    ).toBe(false);
  });
});
