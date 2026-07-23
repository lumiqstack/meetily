import { describe, expect, test } from "bun:test";

import { detectLinkContentMode } from "../../src/lib/link-content-mode";

describe("detectLinkContentMode", () => {
  test("detects a Teams transcript from the id query param", () => {
    const url =
      "https://tenant.sharepoint.com/personal/user/_layouts/15/stream.aspx?id=%2Fpersonal%2Fuser%2FDocuments%2FRecordings%2FWeekly%20Sync%2DMeeting%20Transcript%2Evtt";
    expect(detectLinkContentMode(url)).toBe("transcript");
  });

  test("detects a recording from the id query param", () => {
    const url =
      "https://tenant.sharepoint.com/personal/user/_layouts/15/stream.aspx?id=%2Fpersonal%2Fuser%2FDocuments%2FRecordings%2FWeekly%20Sync%2DRecording%2Emp4";
    expect(detectLinkContentMode(url)).toBe("audio");
  });

  test("falls back to the pathname when there is no id param", () => {
    expect(
      detectLinkContentMode("https://tenant.sharepoint.com/sites/x/Weekly-Recording.mp4")
    ).toBe("audio");
  });

  test("scans the raw string when the value is not a parseable URL", () => {
    expect(detectLinkContentMode("some transcript link")).toBe("transcript");
  });

  test("returns null when it cannot tell", () => {
    expect(detectLinkContentMode("https://tenant.sharepoint.com/sites/x/video.mp4")).toBe(null);
  });
});
