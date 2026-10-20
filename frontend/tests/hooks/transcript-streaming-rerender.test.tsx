import { describe, expect, test } from 'bun:test';
import { act, create } from 'react-test-renderer';
import type { TranscriptSegmentData } from '../../src/types';
import { useTranscriptStreaming } from '../../src/hooks/useTranscriptStreaming';

// The typewriter reveal must finish even when the segment list is replaced
// mid-reveal with the same last segment (a late segment re-sorted in, or the
// reload sync), otherwise the newest line stays truncated on screen.
const wait = (ms: number) => act(() => new Promise<void>(resolve => setTimeout(resolve, ms)));

describe('useTranscriptStreaming', () => {
  test('a new segments array with the same last id mid-reveal still reveals the full text', async () => {
    const latest: TranscriptSegmentData = { id: 'b', timestamp: 2, text: 'x'.repeat(120) };
    const earlier: TranscriptSegmentData = { id: 'a', timestamp: 1, text: 'earlier' };

    let api: ReturnType<typeof useTranscriptStreaming> | undefined;
    function Probe({ segments }: { segments: TranscriptSegmentData[] }) {
      api = useTranscriptStreaming(segments, true, true);
      return null;
    }

    let renderer: ReturnType<typeof create> | undefined;
    await act(async () => {
      renderer = create(<Probe segments={[latest]} />);
    });
    await wait(150);
    expect(api!.getDisplayText(latest).length).toBeLessThan(latest.text.length);

    await act(async () => renderer!.update(<Probe segments={[earlier, latest]} />));
    await wait(1000);

    expect(api!.getDisplayText(latest)).toBe(latest.text);
    await act(async () => renderer!.unmount());
  });
});
