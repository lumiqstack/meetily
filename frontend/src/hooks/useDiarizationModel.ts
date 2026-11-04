import { useCallback, useEffect, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { useTauriEvent } from './useTauriEvent';

/** Mirrors `diarization::engine::ModelReport` (status flattened by serde). */
export type DiarizationModelReport = {
  size_mb: number;
  path: string;
} & (
  | { status: 'missing' }
  | { status: 'downloading'; percent: number }
  | { status: 'corrupted'; file_size: number }
  | { status: 'available' }
);

function errorMessage(err: unknown): string {
  if (typeof err === 'string') return err;
  if (err instanceof Error) return err.message;
  return String(err);
}

/**
 * State of the local speaker model (NVIDIA Nemotron-3 Diarization) and the actions
 * to fetch it. Polls the backend once when `active` turns true and follows the
 * download events afterwards.
 */
export function useDiarizationModel(active: boolean) {
  const [report, setReport] = useState<DiarizationModelReport | null>(null);
  const [downloadPercent, setDownloadPercent] = useState<number | null>(null);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    try {
      const next = await invoke<DiarizationModelReport>('diarization_model_status');
      setReport(next);
      if (next.status === 'downloading') {
        setDownloadPercent(next.percent);
      } else {
        setDownloadPercent(null);
      }
    } catch (err) {
      setError(errorMessage(err));
    }
  }, []);

  useEffect(() => {
    if (active) {
      setError(null);
      refresh();
    }
  }, [active, refresh]);

  useTauriEvent<{ percent: number }>('diarization-download-progress', ({ percent }) => {
    setDownloadPercent(percent);
    setReport((prev) => (prev ? { ...prev, status: 'downloading', percent } : prev));
  });
  useTauriEvent<DiarizationModelReport>('diarization-download-complete', (next) => {
    setDownloadPercent(null);
    setReport(next);
  });
  useTauriEvent<{ error: string }>('diarization-download-failed', ({ error: message }) => {
    setDownloadPercent(null);
    setError(message);
    refresh();
  });

  const download = useCallback(async () => {
    setError(null);
    setDownloadPercent(0);
    try {
      // Resolves when the download finishes; progress arrives through the events above.
      const next = await invoke<DiarizationModelReport>('diarization_download_model');
      setReport(next);
    } catch (err) {
      setError(errorMessage(err));
      refresh();
    } finally {
      setDownloadPercent(null);
    }
  }, [refresh]);

  const cancelDownload = useCallback(async () => {
    try {
      await invoke('diarization_cancel_download');
    } catch (err) {
      console.warn('Could not cancel speaker model download:', err);
    }
  }, []);

  return {
    report,
    isAvailable: report?.status === 'available',
    isDownloading: downloadPercent !== null,
    downloadPercent,
    error,
    refresh,
    download,
    cancelDownload,
  };
}
