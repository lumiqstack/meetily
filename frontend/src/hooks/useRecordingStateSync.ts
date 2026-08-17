import { useState, useEffect, useRef } from 'react';
import { recordingService } from '@/services/recordingService';

interface UseRecordingStateSyncReturn {
  isBackendRecording: boolean;
  isRecordingDisabled: boolean;
  setIsRecordingDisabled: (value: boolean) => void;
}

/**
 * Custom hook for synchronizing frontend recording state with backend.
 *
 * This is a desync safety net (backend recording but UI unaware, e.g. after a
 * reload), not the primary state source — RecordingStateContext polls at 500ms
 * while a recording is active. So a slow poll is enough here.
 *
 * The interval is installed once and reads its inputs through refs. Depending
 * on the caller's props instead would tear down and rebuild the interval on
 * every render of the page, firing an extra IPC round-trip each time.
 */
const POLL_INTERVAL_MS = 2000;

export function useRecordingStateSync(
  isRecording: boolean,
  setIsRecording: (value: boolean) => void,
  setIsMeetingActive: (value: boolean) => void
): UseRecordingStateSyncReturn {
  const [isRecordingDisabled, setIsRecordingDisabled] = useState(false);

  const isRecordingRef = useRef(isRecording);
  const setIsRecordingRef = useRef(setIsRecording);
  const setIsMeetingActiveRef = useRef(setIsMeetingActive);

  isRecordingRef.current = isRecording;
  setIsRecordingRef.current = setIsRecording;
  setIsMeetingActiveRef.current = setIsMeetingActive;

  useEffect(() => {
    if (typeof window === 'undefined' || !(window as any).__TAURI__) {
      return;
    }

    let cancelled = false;

    const checkRecordingState = async () => {
      try {
        const isCurrentlyRecording = await recordingService.isRecording();
        if (cancelled) return;

        const uiRecording = isRecordingRef.current;
        if (isCurrentlyRecording && !uiRecording) {
          console.log('Recording active in backend but not in UI, synchronizing state...');
          setIsRecordingRef.current(true);
          setIsMeetingActiveRef.current(true);
        } else if (!isCurrentlyRecording && uiRecording) {
          console.log('Recording inactive in backend but active in UI, synchronizing state...');
          setIsRecordingRef.current(false);
        }
      } catch (error) {
        console.error('Failed to check recording state:', error);
      }
    };

    checkRecordingState();
    const interval = setInterval(checkRecordingState, POLL_INTERVAL_MS);

    return () => {
      cancelled = true;
      clearInterval(interval);
    };
  }, []);

  return {
    isBackendRecording: isRecording,
    isRecordingDisabled,
    setIsRecordingDisabled,
  };
}
