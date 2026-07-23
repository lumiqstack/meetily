import { describe, expect, test } from "bun:test";

import React from "react";
import { renderToStaticMarkup } from "react-dom/server";

import { VirtualizedTranscriptView } from "../../src/components/VirtualizedTranscriptView";
import { TooltipProvider } from "../../src/components/ui/tooltip";
import type { TranscriptSegmentData } from "../../src/types";

// Regression test for the wiring bug where TranscriptSegment accepted a
// `speaker` prop but the call sites never passed it, so imported Teams
// transcripts rendered without their speaker labels.
describe("transcript speaker rendering", () => {
  test("renders the speaker label when a segment carries one", () => {
    const segments: TranscriptSegmentData[] = [
      {
        id: "s1",
        text: "Thanks everyone for joining.",
        timestamp: 3.1,
        speaker: "Jane Smith",
      },
      {
        id: "s2",
        text: "Recorded without attribution.",
        timestamp: 7.0,
        speaker: null,
      },
    ];

    const html = renderToStaticMarkup(
      <TooltipProvider>
        <VirtualizedTranscriptView segments={segments} />
      </TooltipProvider>
    );

    expect(html).toContain("Jane Smith");
    expect(html).toContain("Thanks everyone for joining.");
    expect(html).toContain("Recorded without attribution.");
  });
});
