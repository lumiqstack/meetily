'use client';

import React, { useEffect } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { appDataDir } from '@tauri-apps/api/path';
import { toast } from 'sonner';
import { useRecordingStop } from '@/hooks/useRecordingStop';
import { useTauriEvent } from '@/hooks/useTauriEvent';
import { handleRecordingError } from '@/lib/recording-error';
import { recordingService } from '@/services/recordingService';

/**
 * RecordingPostProcessingProvider
 *
 * This provider handles post-processing when recording stops from any source:
 * - Tray menu stop
 * - Global keyboard shortcut
 * - Overlay stop button
 * - Main UI stop button
 *
 * It listens for the 'recording-stop-complete' event from Rust backend
 * and triggers the full post-processing flow (save to database, navigate, analytics)
 * regardless of which page the user is currently on.
 *
 * It also listens for 'recording-error': when the backend stops capture after
 * too many audio errors, it runs the same stop flow so the UI does not keep
 * showing a live recording.
 */
export function RecordingPostProcessingProvider({ children }: { children: React.ReactNode }) {
  // No-op functions since the global RecordingStateContext already handles state updates
  // These are only needed for the hook's local component state management
  const setIsRecording = () => { };
  const setIsRecordingDisabled = () => { };

  const {
    handleRecordingStop,
  } = useRecordingStop(setIsRecording, setIsRecordingDisabled, { exposeToWindow: true });

  useTauriEvent<boolean>('recording-stop-complete', (callApi) => {
    console.log('[RecordingPostProcessing] Received recording-stop-complete event:', callApi);
    // callApi is true for normal stops
    handleRecordingStop(callApi);
  });

  useEffect(() => {
    let unlistenFn: (() => void) | undefined;
    let cancelled = false;

    recordingService
      .onRecordingError((payload) => {
        void handleRecordingError(payload, {
          toastError: (title, options) => toast.error(title, options),
          stopRecording: async () => {
            const dataDir = await appDataDir();
            const timestamp = new Date().toISOString().replace(/[:.]/g, '-');
            await invoke('stop_recording', {
              args: { save_path: `${dataDir}/recording-${timestamp}.wav` },
            });
          },
          finishStop: () => handleRecordingStop(true),
        });
      })
      .then((fn) => {
        if (cancelled) fn();
        else unlistenFn = fn;
      })
      .catch((error) => {
        console.error('[RecordingPostProcessing] Failed to set up recording-error listener:', error);
      });

    return () => {
      cancelled = true;
      unlistenFn?.();
    };
  }, [handleRecordingStop]);

  return <>{children}</>;
}
