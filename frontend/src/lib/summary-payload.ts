import { invoke as invokeTauri } from '@tauri-apps/api/core';
import { toast } from 'sonner';
import { Transcript } from '@/types';
import { displaySpeaker } from '@/lib/speaker-label';
import {
  detectAndCacheSummaryLanguage,
  readMeetingSummaryLanguage,
  readCachedDetectedSummaryLanguage,
} from '@/lib/summary-language-preferences';

/**
 * Shared helpers for kicking off AI summary generation. Extracted from
 * useSummaryGeneration so the home-screen PendingWorkPanel formats the
 * transcript payload and resolves language identically to meeting-details.
 */

export async function resolveSummaryLanguage(
  meetingId: string,
  transcriptTexts: string[]
): Promise<string | null> {
  try {
    const perMeeting = await readMeetingSummaryLanguage(meetingId);
    if (perMeeting.language) return perMeeting.language;
  } catch (err) {
    console.warn('Failed to load meeting summary language:', err);
    toast.warning('Could not load saved summary language', {
      description: 'Using Auto for this generation.',
    });
  }

  try {
    const cachedDetected = await readCachedDetectedSummaryLanguage(meetingId);
    if (cachedDetected) return cachedDetected;
  } catch (err) {
    console.warn('Failed to load cached detected summary language:', err);
  }

  try {
    const detection = await detectAndCacheSummaryLanguage(meetingId, transcriptTexts);
    if (detection.reason === 'tie') {
      toast.warning('Bilingual transcript detected', {
        description: 'Pick a summary language manually if Auto chooses the wrong fallback.',
      });
    }
    return detection.language;
  } catch (err) {
    console.warn('Failed to detect transcript summary language:', err);
    return null;
  }
}

/** Fetch ALL transcripts for a meeting (not just the paginated view). */
export async function fetchAllTranscripts(meetingId: string): Promise<Transcript[]> {
  try {
    const firstPage = await invokeTauri('api_get_meeting_transcripts', {
      meetingId,
      limit: 1,
      offset: 0,
    }) as { transcripts: Transcript[]; total_count: number; has_more: boolean };

    if (firstPage.total_count === 0) {
      return [];
    }

    const allData = await invokeTauri('api_get_meeting_transcripts', {
      meetingId,
      limit: firstPage.total_count,
      offset: 0,
    }) as { transcripts: Transcript[]; total_count: number; has_more: boolean };

    return allData.transcripts;
  } catch (error) {
    console.error('❌ Error fetching all transcripts:', error);
    toast.error('Failed to fetch transcripts for summary generation');
    return [];
  }
}

export function buildSummaryTranscriptPayload(allTranscripts: Transcript[]) {
  const formatTime = (seconds: number | undefined, fallbackTimestamp: string): string => {
    if (seconds === undefined) {
      return fallbackTimestamp;
    }
    const totalSecs = Math.floor(seconds);
    const mins = Math.floor(totalSecs / 60);
    const secs = totalSecs % 60;
    return `[${mins.toString().padStart(2, '0')}:${secs.toString().padStart(2, '0')}]`;
  };

  return {
    transcriptText: allTranscripts
      .map(t => {
        const speaker = displaySpeaker(t.speaker);
        return `${formatTime(t.audio_start_time, t.timestamp)} ${speaker ? `${speaker}: ` : ''}${t.text}`;
      })
      .join('\n'),
    transcriptTexts: allTranscripts.map(t => t.text),
  };
}
