import { useCallback, useEffect, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { toast } from 'sonner';
import { errorMessage } from '@/lib/utils';
import { MeetingSpeaker, planSpeakerRename, renameToast } from '@/lib/speaker-names';

interface UseSpeakerNamesProps {
  meetingId?: string;
  /** Whether the meeting already has a summary, which a rename does not rewrite. */
  hasSummary: boolean;
  /** Called after the backend rewrote `oldSpeaker` to `newSpeaker` on every segment. */
  onRenamed: (oldSpeaker: string, newSpeaker: string) => void | Promise<void>;
}

export interface SpeakerNames {
  speakers: MeetingSpeaker[];
  refresh: () => Promise<void>;
  /** Saves `input` as the name of `oldSpeaker` (raw value). Resolves true when the dialog may close. */
  rename: (oldSpeaker: string, input: string) => Promise<boolean>;
}

export function useSpeakerNames({ meetingId, hasSummary, onRenamed }: UseSpeakerNamesProps): SpeakerNames {
  const [speakers, setSpeakers] = useState<MeetingSpeaker[]>([]);
  const requestRef = useRef(0);

  const refresh = useCallback(async () => {
    const request = ++requestRef.current;
    if (!meetingId) {
      setSpeakers([]);
      return;
    }
    try {
      const rows = await invoke<MeetingSpeaker[]>('api_get_meeting_speakers', { meetingId });
      if (request === requestRef.current) setSpeakers(rows);
    } catch (error) {
      console.error('Failed to load meeting speakers:', error);
    }
  }, [meetingId]);

  useEffect(() => {
    setSpeakers([]);
    void refresh();
  }, [refresh]);

  const rename = useCallback(async (oldSpeaker: string, input: string) => {
    if (!meetingId) return false;
    const plan = planSpeakerRename(oldSpeaker, input, speakers.map((s) => s.speaker));
    if (plan.kind === 'unchanged') return true;
    if (plan.kind === 'invalid') {
      toast.error(plan.reason);
      return false;
    }

    try {
      const changed = await invoke<number>('api_rename_meeting_speaker', {
        meetingId,
        oldSpeaker,
        newSpeaker: plan.newSpeaker,
      });
      await onRenamed(oldSpeaker, plan.newSpeaker);
      void refresh();
      const { title, description } = renameToast(oldSpeaker, plan, changed, hasSummary);
      toast.success(title, { description });
      return true;
    } catch (error) {
      toast.error('Failed to rename speaker', { description: errorMessage(error) });
      return false;
    }
  }, [meetingId, speakers, onRenamed, refresh, hasSummary]);

  return { speakers, refresh, rename };
}
