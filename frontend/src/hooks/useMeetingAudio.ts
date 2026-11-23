import { useCallback, useEffect, useRef, useState } from 'react';
import { listen } from '@tauri-apps/api/event';
import { toast } from 'sonner';
import { meetingAudioStatus, purgeRecordings, purgeResultText, type AudioStatus } from '@/lib/recordings';

/** The audio status of one meeting, kept fresh on `meetings-changed`. */
export function useMeetingAudio(meetingId?: string) {
  const [audio, setAudio] = useState<AudioStatus>();
  const latest = useRef(0);

  const refresh = useCallback(async () => {
    const request = ++latest.current;
    if (!meetingId) {
      setAudio(undefined);
      return;
    }
    try {
      const status = await meetingAudioStatus(meetingId);
      if (request === latest.current) setAudio(status);
    } catch (error) {
      console.error('Failed to read meeting audio status:', error);
      if (request === latest.current) setAudio(undefined);
    }
  }, [meetingId]);

  useEffect(() => {
    void refresh();
    let unlisten: (() => void) | undefined;
    let disposed = false;
    listen<{ meeting_id?: string }>('meetings-changed', (event) => {
      if (!event.payload?.meeting_id || event.payload.meeting_id === meetingId) void refresh();
    }).then((fn) => {
      if (disposed) fn();
      else unlisten = fn;
    }).catch((error) => console.error('Failed to listen for meetings-changed:', error));
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [meetingId, refresh]);

  const removeAudio = useCallback(async () => {
    if (!meetingId) return;
    try {
      const report = await purgeRecordings({ meetingIds: [meetingId], orphanFolders: [] });
      const skipped = report.skipped[0];
      if (skipped) {
        toast.error('Audio was not removed', { description: skipped.reason.text });
      } else if (report.failed.length > 0) {
        toast.error('Some audio files could not be removed', {
          description: 'A file may be open in another app. Try again later.',
        });
      } else {
        toast.success(purgeResultText(report).title, { description: 'The transcript and summary are kept.' });
      }
    } catch (error) {
      toast.error('Audio was not removed', { description: error instanceof Error ? error.message : String(error) });
    }
    await refresh();
  }, [meetingId, refresh]);

  return { audio, removeAudio };
}
