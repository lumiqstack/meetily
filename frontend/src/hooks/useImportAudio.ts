import { useState, useEffect, useCallback, useRef } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { listen, UnlistenFn } from '@tauri-apps/api/event';
import Analytics from '@/lib/analytics';
import { applyPinnedSummaryLanguageToMeeting } from '@/lib/summary-language-preferences';
import { toast } from 'sonner';

export interface AudioFileInfo {
  path: string;
  filename: string;
  duration_seconds: number;
  size_bytes: number;
  format: string;
}

export interface ImportProgress {
  import_id: string;
  stage: string;
  progress_percentage: number;
  message: string;
}

export interface ImportResult {
  import_id: string;
  meeting_id: string;
  title: string;
  segments_count: number;
  duration_seconds: number;
}

export interface ImportError {
  import_id: string;
  error: string;
}

export interface ImportStarted {
  import_id: string;
  message: string;
}

function createClientImportId(): string {
  if (typeof crypto !== 'undefined' && typeof crypto.randomUUID === 'function') {
    return `import-${crypto.randomUUID()}`;
  }

  return `import-${Date.now()}-${Math.random().toString(36).slice(2)}`;
}

export type ImportStatus = 'idle' | 'validating' | 'processing' | 'complete' | 'error';

export interface UseImportAudioOptions {
  onComplete?: (result: ImportResult) => void;
  onError?: (error: string) => void;
}

export interface UseImportAudioReturn {
  status: ImportStatus;
  fileInfo: AudioFileInfo | null;
  progress: ImportProgress | null;
  error: string | null;
  isProcessing: boolean;
  isBusy: boolean;
  selectFile: () => Promise<AudioFileInfo | null>;
  validateFile: (path: string) => Promise<AudioFileInfo | null>;
  startImport: (
    sourcePath: string,
    title: string,
    language?: string | null,
    model?: string | null,
    provider?: string | null
  ) => Promise<ImportStarted | null>;
  cancelImport: () => Promise<void>;
  reset: () => void;
}

export function useImportAudio({
  onComplete,
  onError,
}: UseImportAudioOptions = {}): UseImportAudioReturn {
  const [status, setStatus] = useState<ImportStatus>('idle');
  const [fileInfo, setFileInfo] = useState<AudioFileInfo | null>(null);
  const [progress, setProgress] = useState<ImportProgress | null>(null);
  const [error, setError] = useState<string | null>(null);

  // Stable refs for callbacks to avoid listener re-registration on every render
  const onCompleteRef = useRef(onComplete);
  const onErrorRef = useRef(onError);
  useEffect(() => { onCompleteRef.current = onComplete; }, [onComplete]);
  useEffect(() => { onErrorRef.current = onError; }, [onError]);

  // Active backend job ID for this hook instance. Used to ignore events from
  // other simultaneous imports.
  const activeImportIdRef = useRef<string | null>(null);

  // Cancellation guard: prevents late events from updating state after cancel
  const isCancelledRef = useRef(false);

  // Set up event listeners (registered once, use refs for callbacks)
  useEffect(() => {
    const unlisteners: UnlistenFn[] = [];
    const cleanedUpRef = { current: false };

    const setupListeners = async () => {
      // Progress events
      const unlistenProgress = await listen<ImportProgress>(
        'import-progress',
        (event) => {
          if (
            isCancelledRef.current ||
            !activeImportIdRef.current ||
            event.payload.import_id !== activeImportIdRef.current
          ) {
            return;
          }

          setProgress(event.payload);
          setStatus('processing');
        }
      );
      if (cleanedUpRef.current) {
        unlistenProgress();
        return;
      }
      unlisteners.push(unlistenProgress);

      // Completion event
      const unlistenComplete = await listen<ImportResult>(
        'import-complete',
        async (event) => {
          if (
            isCancelledRef.current ||
            !activeImportIdRef.current ||
            event.payload.import_id !== activeImportIdRef.current
          ) {
            return;
          }

          activeImportIdRef.current = null;

          await Analytics.track('import_audio_completed', {
            success: 'true',
            duration_seconds: event.payload.duration_seconds.toString(),
            segments_count: event.payload.segments_count.toString()
          });

          setStatus('complete');
          setProgress(null);
          try {
            await applyPinnedSummaryLanguageToMeeting(event.payload.meeting_id);
          } catch (error) {
            console.warn('Failed to apply pinned summary language to imported meeting:', error);
            toast.warning('Could not apply default summary language', {
              description: 'The imported meeting was saved, but the default summary language was not applied.',
            });
          }
          onCompleteRef.current?.(event.payload);
        }
      );
      if (cleanedUpRef.current) {
        unlistenComplete();
        unlisteners.forEach(u => u());
        return;
      }
      unlisteners.push(unlistenComplete);

      // Error event
      const unlistenError = await listen<ImportError>(
        'import-error',
        async (event) => {
          if (
            isCancelledRef.current ||
            !activeImportIdRef.current ||
            event.payload.import_id !== activeImportIdRef.current
          ) {
            return;
          }

          activeImportIdRef.current = null;

          await Analytics.trackError('import_audio_failed', event.payload.error);

          setStatus('error');
          setError(event.payload.error);
          onErrorRef.current?.(event.payload.error);
        }
      );
      if (cleanedUpRef.current) {
        unlistenError();
        unlisteners.forEach(u => u());
        return;
      }
      unlisteners.push(unlistenError);
    };

    setupListeners();

    return () => {
      cleanedUpRef.current = true;
      unlisteners.forEach((unlisten) => unlisten());
    };
  }, []);

  // Select file using native file dialog
  const selectFile = useCallback(async (): Promise<AudioFileInfo | null> => {
    setStatus('validating');
    setError(null);

    try {
      const result = await invoke<AudioFileInfo | null>('select_and_validate_audio_command');
      if (result) {
        setFileInfo(result);
        setStatus('idle');
        return result;
      } else {
        // User cancelled
        setStatus('idle');
        return null;
      }
    } catch (err: any) {
      setStatus('error');
      const errorMsg = typeof err === 'string' ? err : (err?.message || String(err) || 'Failed to validate file');
      setError(errorMsg);
      onErrorRef.current?.(errorMsg);
      return null;
    }
  }, []);

  // Validate a file from a given path (for drag-drop)
  const validateFile = useCallback(async (path: string): Promise<AudioFileInfo | null> => {
    setStatus('validating');
    setError(null);

    try {
      const result = await invoke<AudioFileInfo>('validate_audio_file_command', { path });
      setFileInfo(result);
      setStatus('idle');
      return result;
    } catch (err: any) {
      setStatus('error');
      const errorMsg = typeof err === 'string' ? err : (err?.message || String(err) || 'Failed to validate file');
      setError(errorMsg);
      onErrorRef.current?.(errorMsg);
      return null;
    }
  }, []);

  // Start the import process
  const startImport = useCallback(
    async (
      sourcePath: string,
      title: string,
      language?: string | null,
      model?: string | null,
      provider?: string | null
    ) => {
      const importId = createClientImportId();

      activeImportIdRef.current = importId;
      isCancelledRef.current = false;
      setStatus('processing');
      setError(null);
      setProgress(null);

      try {
        if (fileInfo) {
          await Analytics.track('import_audio_started', {
            file_size_bytes: fileInfo.size_bytes.toString(),
            duration_seconds: fileInfo.duration_seconds.toString(),
            language: language || 'auto',
            model_provider: provider || '',
            model_name: model || ''
          });
        }

        const started = await invoke<ImportStarted>('start_import_audio_command', {
          importId,
          sourcePath,
          title,
          language: language || null,
          model: model || null,
          provider: provider || null,
        });

        activeImportIdRef.current = started.import_id;
        return started;
      } catch (err: any) {
        if (activeImportIdRef.current === importId) {
          activeImportIdRef.current = null;
        }

        setStatus('error');
        const errorMsg = typeof err === 'string' ? err : (err?.message || String(err) || 'Failed to start import');
        setError(errorMsg);

        await Analytics.trackError('import_audio_failed', errorMsg);

        onErrorRef.current?.(errorMsg);
        return null;
      }
    },
    [fileInfo]
  );

  // Cancel ongoing import
  const cancelImport = useCallback(async () => {
    const importId = activeImportIdRef.current;
    isCancelledRef.current = true;

    try {
      if (importId) {
        await invoke('cancel_import_command', { importId });
      }
    } catch (err: any) {
      console.error('Failed to cancel import:', err);
    } finally {
      if (!importId || activeImportIdRef.current === importId) {
        activeImportIdRef.current = null;
      }

      setStatus('idle');
      setProgress(null);
    }
  }, []);

  // Reset all state
  const reset = useCallback(() => {
    activeImportIdRef.current = null;
    isCancelledRef.current = false;
    setStatus('idle');
    setFileInfo(null);
    setProgress(null);
    setError(null);
  }, []);

  return {
    status,
    fileInfo,
    progress,
    error,
    isProcessing: status === 'processing',
    isBusy: status === 'processing' || status === 'validating',
    selectFile,
    validateFile,
    startImport,
    cancelImport,
    reset,
  };
}
