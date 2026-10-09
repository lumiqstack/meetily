'use client';

import React, { useEffect } from 'react';
import { listen } from '@tauri-apps/api/event';
import { invoke } from '@tauri-apps/api/core';
import { appDataDir } from '@tauri-apps/api/path';
import { toast } from 'sonner';
import { useRecordingStop } from '@/hooks/useRecordingStop';
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

  useEffect(() => {
    let unlistenFn: (() => void) | undefined;

    const setupListener = async () => {
      try {
        // Listen for recording-stop-complete event from Rust
        unlistenFn = await listen<boolean>('recording-stop-complete', (event) => {
          console.log('[RecordingPostProcessing] Received recording-stop-complete event:', event.payload);

          // Call the post-processing handler
          // event.payload is the callApi boolean (true for normal stops)
          handleRecordingStop(event.payload);
        });

        console.log('[RecordingPostProcessing] Event listener set up successfully');
      } catch (error) {
        console.error('[RecordingPostProcessing] Failed to set up event listener:', error);
      }
    };

    setupListener();

    return () => {
      if (unlistenFn) {
        console.log('[RecordingPostProcessing] Cleaning up event listener');
        unlistenFn();
      }
    };
  }, [handleRecordingStop]);

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
