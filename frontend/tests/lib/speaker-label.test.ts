import { describe, expect, test } from "bun:test";

import { displaySpeaker } from "../../src/lib/speaker-label";

describe("displaySpeaker", () => {
  test("maps live-recording source tags to Me/Others", () => {
    expect(displaySpeaker("mic")).toBe("Me");
    expect(displaySpeaker("system")).toBe("Others");
  });

  test("passes imported speaker names through verbatim", () => {
    expect(displaySpeaker("Jane Smith")).toBe("Jane Smith");
    // A real participant who happens to be named like a tag, but cased
    // differently, is not remapped.
    expect(displaySpeaker("Mic")).toBe("Mic");
  });

  test("returns null for missing speakers", () => {
    expect(displaySpeaker(null)).toBeNull();
    expect(displaySpeaker(undefined)).toBeNull();
    expect(displaySpeaker("")).toBeNull();
  });
});
