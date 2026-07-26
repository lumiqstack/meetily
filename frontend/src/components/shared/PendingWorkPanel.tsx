'use client';

import React, { useCallback, useEffect, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { toast } from 'sonner';
import { Loader2, Trash2 } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { ConfirmationModal } from '@/components/ConfirmationModel/confirmation-modal';
import { useConfig } from '@/contexts/ConfigContext';
import { useSidebar } from '@/components/Sidebar/SidebarProvider';
import { backgroundJobStore } from './BackgroundJobToast';
import { summaryJobId } from '@/lib/background-jobs';
import {
  buildSummaryTranscriptPayload,
  fetchAllTranscripts,
  resolveSummaryLanguage,
} from '@/lib/summary-payload';

interface PendingMeetingResponse {
  id: string;
  title: string;
  created_at: string;
  folder_path: string | null;
  transcript_count: number;
  summary_status: string | null;
}

type PendingKind = 'transcription' | 'summary';

interface PendingItem {
  meetingId: string;
  title: string;
  createdAt: string;
  folderPath: string | null;
  kind: PendingKind;
}

const SUMMARY_POLL_INTERVAL_MS = 5000;
const SUMMARY_POLL_MAX_ATTEMPTS = 200;
/** Fallback cadence for detecting a finished retranscription if events are missed. */
const RETRANSCRIPTION_FALLBACK_POLL_MS = 20000;

const sleep = (ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms));

/**
 * Wait for a retranscription started by this panel to reach a terminal state.
 * Listeners are armed before the command is invoked, so completion can't be
 * missed; a slow poll of the backend job flag is kept as a safety net.
 */
function waitForRetranscription(meetingId: string): {
  promise: Promise<{ ok: boolean; error?: string }>;
  cancel: () => void;
} {
  let settled = false;
  const cleanupFns: Array<() => void> = [];
  const cleanup = () => {
    cleanupFns.splice(0).forEach((fn) => fn());
  };

  const promise = new Promise<{ ok: boolean; error?: string }>((resolve) => {
    const settle = (ok: boolean, error?: string) => {
      if (settled) return;
      settled = true;
      cleanup();
      resolve({ ok, error });
    };

    listen<{ meeting_id: string }>('retranscription-complete', (event) => {
      if (event.payload.meeting_id === meetingId) settle(true);
    }).then((unlisten) => (settled ? unlisten() : cleanupFns.push(unlisten)));

    listen<{ meeting_id: string; error: string }>('retranscription-error', (event) => {
      if (event.payload.meeting_id === meetingId) settle(false, event.payload.error);
    }).then((unlisten) => (settled ? unlisten() : cleanupFns.push(unlisten)));

    const interval = setInterval(async () => {
      try {
        const inProgress = await invoke<boolean>('is_retranscription_in_progress_command');
        if (inProgress) return;
        // Job flag is down and no event arrived: infer the outcome from
        // whether transcripts now exist.
        const page = (await invoke('api_get_meeting_transcripts', {
          meetingId,
          limit: 1,
          offset: 0,
        })) as { total_count: number };
        if (page.total_count > 0) {
          settle(true);
        } else {
          settle(false, 'Transcription finished without producing any transcripts');
        }
      } catch {
        // Keep waiting; the next tick or an event will settle it.
      }
    }, RETRANSCRIPTION_FALLBACK_POLL_MS);
    cleanupFns.push(() => clearInterval(interval));
  });

  return {
    promise,
    cancel: () => {
      settled = true;
      cleanup();
    },
  };
}

/**
 * Home-screen panel listing meetings with outstanding work — a recording
 * without a transcript, or a transcript without an AI summary. The user picks
 * any subset and Process runs them sequentially in the background, surfacing
 * progress through the shared background-jobs store. A meeting needing both
 * is fully processed in one go: transcription first, then summary.
 */
export function PendingWorkPanel() {
  const { modelConfig, transcriptModelConfig } = useConfig();
  const { refetchMeetings } = useSidebar();
  const [items, setItems] = useState<PendingItem[]>([]);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [failures, setFailures] = useState<Map<string, string>>(new Map());
  const [deleteTarget, setDeleteTarget] = useState<PendingItem | null>(null);
  const [isProcessing, setIsProcessing] = useState(false);
  const processingRef = useRef(false);

  const markFailed = useCallback((meetingId: string, message: string) => {
    setFailures((prev) => new Map(prev).set(meetingId, message));
  }, []);

  const clearFailed = useCallback((meetingId: string) => {
    setFailures((prev) => {
      if (!prev.has(meetingId)) return prev;
      const next = new Map(prev);
      next.delete(meetingId);
      return next;
    });
  }, []);

  const refresh = useCallback(async () => {
    try {
      const pending = await invoke<PendingMeetingResponse[]>('api_get_pending_meetings');
      const mapped: PendingItem[] = pending.map((m) => ({
        meetingId: m.id,
        title: m.title,
        createdAt: m.created_at,
        folderPath: m.folder_path,
        kind: m.transcript_count === 0 ? 'transcription' : 'summary',
      }));
      setItems(mapped);
      const valid = new Set(mapped.map((i) => i.meetingId));
      setSelected((prev) => new Set([...prev].filter((id) => valid.has(id))));
      setFailures((prev) => {
        if (![...prev.keys()].some((id) => !valid.has(id))) return prev;
        return new Map([...prev].filter(([id]) => valid.has(id)));
      });
    } catch (error) {
      console.warn('Failed to load pending meetings:', error);
    }
  }, []);

  useEffect(() => {
    refresh();
    // Retranscriptions finishing anywhere in the app (dialog, retry, this
    // panel) resolve a pending item, so refresh the list.
    const handleRetranscriptionComplete = () => {
      refresh();
    };
    window.addEventListener(
      'meetily-background-retranscription-complete',
      handleRetranscriptionComplete
    );
    return () => {
      window.removeEventListener(
        'meetily-background-retranscription-complete',
        handleRetranscriptionComplete
      );
    };
  }, [refresh]);

  const toggleItem = useCallback((meetingId: string) => {
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(meetingId)) {
        next.delete(meetingId);
      } else {
        next.add(meetingId);
      }
      return next;
    });
  }, []);

  const allSelected = items.length > 0 && items.every((i) => selected.has(i.meetingId));
  const toggleAll = useCallback(() => {
    setSelected(allSelected ? new Set() : new Set(items.map((i) => i.meetingId)));
  }, [allSelected, items]);

  const runTranscription = useCallback(
    async (item: PendingItem): Promise<boolean> => {
      if (!item.folderPath) {
        backgroundJobStore.registerRetranscription(item.meetingId, item.title);
        backgroundJobStore.applyError(item.meetingId, 'Meeting folder path not available');
        markFailed(item.meetingId, 'Meeting folder path not available');
        return false;
      }

      backgroundJobStore.registerRetranscription(item.meetingId, item.title);
      const waiter = waitForRetranscription(item.meetingId);
      try {
        await invoke('start_retranscription_command', {
          meetingId: item.meetingId,
          meetingFolderPath: item.folderPath,
          language: null,
          model: transcriptModelConfig.model || null,
          provider: transcriptModelConfig.provider || null,
        });
      } catch (err) {
        waiter.cancel();
        const message = typeof err === 'string' ? err : err instanceof Error ? err.message : String(err);
        backgroundJobStore.applyError(item.meetingId, message);
        markFailed(item.meetingId, message);
        return false;
      }
      const result = await waiter.promise;
      if (!result.ok) {
        markFailed(item.meetingId, result.error ?? 'Transcription failed');
      }
      return result.ok;
    },
    [transcriptModelConfig, markFailed]
  );

  const runSummary = useCallback(
    async (item: PendingItem): Promise<boolean> => {
      const jobId = summaryJobId(item.meetingId);
      backgroundJobStore.registerSummary(item.meetingId, item.title);
      const fail = (message: string): false => {
        backgroundJobStore.applyError(jobId, message);
        markFailed(item.meetingId, message);
        return false;
      };
      try {
        const transcripts = await fetchAllTranscripts(item.meetingId);
        if (!transcripts.length) {
          throw new Error('No transcripts available for summary');
        }
        const payload = buildSummaryTranscriptPayload(transcripts);
        const summaryLanguage = await resolveSummaryLanguage(
          item.meetingId,
          payload.transcriptTexts
        );

        backgroundJobStore.applyProgress(jobId, 5, 'Starting summary generation...');
        await invoke('api_process_transcript', {
          text: payload.transcriptText,
          model: modelConfig.provider,
          modelName: modelConfig.model,
          meetingId: item.meetingId,
          chunkSize: 40000,
          overlap: 1000,
          customPrompt: '',
          templateId: 'standard_meeting',
          summaryLanguage,
        });

        for (let attempt = 0; attempt < SUMMARY_POLL_MAX_ATTEMPTS; attempt++) {
          await sleep(SUMMARY_POLL_INTERVAL_MS);
          const result = (await invoke('api_get_summary', {
            meetingId: item.meetingId,
          })) as { status: string; error?: string | null };
          const status = (result.status || '').toLowerCase();

          if (status === 'completed') {
            backgroundJobStore.applyProgress(jobId, 100, 'Summary ready');
            backgroundJobStore.applyComplete(jobId);
            return true;
          }
          if (status === 'failed' || status === 'error') {
            return fail(result.error || 'Summary generation failed');
          }
          if (status === 'cancelled') {
            return fail('Summary generation cancelled');
          }
          if (status === 'idle') {
            return fail('Summary process not found');
          }
          backgroundJobStore.applyProgress(
            jobId,
            Math.min(90, 5 + attempt * 4),
            'Generating summary...'
          );
        }
        return fail('Timed out waiting for summary');
      } catch (err) {
        const message = typeof err === 'string' ? err : err instanceof Error ? err.message : String(err);
        return fail(message);
      }
    },
    [modelConfig, markFailed]
  );

  const handleProcess = useCallback(async () => {
    if (processingRef.current) return;
    const chosen = items.filter((i) => selected.has(i.meetingId));
    if (!chosen.length) return;

    processingRef.current = true;
    setIsProcessing(true);
    try {
      for (const item of chosen) {
        clearFailed(item.meetingId);
        if (item.kind === 'transcription') {
          const transcribed = await runTranscription(item);
          // One click fully processes the meeting: chain the summary once
          // the fresh transcript exists.
          if (transcribed) {
            await runSummary(item);
          }
        } else {
          await runSummary(item);
        }
        await refresh();
      }
    } catch (error) {
      console.error('Pending work processing failed:', error);
      toast.error('Processing pending work failed', {
        description: error instanceof Error ? error.message : String(error),
      });
    } finally {
      processingRef.current = false;
      setIsProcessing(false);
      setSelected(new Set());
      await refresh();
    }
  }, [items, selected, runTranscription, runSummary, refresh, clearFailed]);

  const handleDeleteConfirm = useCallback(async () => {
    if (!deleteTarget) return;
    const { meetingId } = deleteTarget;
    setDeleteTarget(null);
    try {
      await invoke('api_delete_meeting', { meetingId });
      clearFailed(meetingId);
      setSelected((prev) => {
        const next = new Set(prev);
        next.delete(meetingId);
        return next;
      });
      await refresh();
      await refetchMeetings();
      toast.success('Meeting deleted');
    } catch (error) {
      toast.error('Failed to delete meeting', {
        description: error instanceof Error ? error.message : String(error),
      });
    }
  }, [deleteTarget, clearFailed, refresh, refetchMeetings]);

  if (items.length === 0 && !isProcessing) {
    return null;
  }

  return (
    <div className="flex justify-center px-4 pt-4">
      <div className="w-2/3 max-w-[750px]">
        <p className="text-xs font-medium text-gray-500 uppercase tracking-wide mb-2">
          Pending work
        </p>
        <div className="bg-white rounded-lg shadow-sm border border-gray-200 p-3">
          {items.length > 1 && (
            <label className="flex items-center gap-2 pb-2 mb-1 border-b border-gray-100 text-xs text-gray-500 cursor-pointer select-none">
              <input
                type="checkbox"
                checked={allSelected}
                onChange={toggleAll}
                disabled={isProcessing}
                className="h-4 w-4 rounded border-gray-300 accent-gray-900"
              />
              Select all
            </label>
          )}
          <div className="space-y-1">
            {items.map((item) => {
              const failure = failures.get(item.meetingId);
              return (
                <label
                  key={item.meetingId}
                  className="flex items-center gap-2 py-1.5 cursor-pointer select-none"
                >
                  <input
                    type="checkbox"
                    checked={selected.has(item.meetingId)}
                    onChange={() => toggleItem(item.meetingId)}
                    disabled={isProcessing}
                    className="h-4 w-4 rounded border-gray-300 accent-gray-900"
                  />
                  <span className="flex-1 min-w-0">
                    <span className="block text-sm text-gray-900 truncate">
                      {item.title}
                    </span>
                    {failure && (
                      <span
                        className="block text-[11px] text-red-600 truncate"
                        title={failure}
                      >
                        {failure}
                      </span>
                    )}
                  </span>
                  {failure ? (
                    <span className="flex-shrink-0 text-[11px] font-medium px-2 py-0.5 rounded-full bg-red-50 text-red-700">
                      Failed
                    </span>
                  ) : (
                    <span
                      className={`flex-shrink-0 text-[11px] font-medium px-2 py-0.5 rounded-full ${
                        item.kind === 'transcription'
                          ? 'bg-blue-50 text-blue-700'
                          : 'bg-amber-50 text-amber-700'
                      }`}
                    >
                      {item.kind === 'transcription' ? 'Needs transcript' : 'Needs summary'}
                    </span>
                  )}
                  <span className="flex-shrink-0 text-xs text-gray-400">
                    {new Date(item.createdAt).toLocaleDateString()}
                  </span>
                  {failure && (
                    <button
                      type="button"
                      onClick={(e) => {
                        e.preventDefault();
                        e.stopPropagation();
                        setDeleteTarget(item);
                      }}
                      disabled={isProcessing}
                      className="flex-shrink-0 p-1 text-gray-400 hover:text-red-600 hover:bg-red-50 rounded transition-colors disabled:opacity-50"
                      title="Remove meeting"
                    >
                      <Trash2 className="w-4 h-4" />
                    </button>
                  )}
                </label>
              );
            })}
          </div>
          <div className="flex justify-end pt-2 mt-1 border-t border-gray-100">
            <Button
              size="sm"
              onClick={handleProcess}
              disabled={selected.size === 0 || isProcessing}
            >
              {isProcessing ? (
                <>
                  <Loader2 className="w-4 h-4 animate-spin" />
                  Processing...
                </>
              ) : (
                `Process${selected.size > 0 ? ` (${selected.size})` : ''}`
              )}
            </Button>
          </div>
        </div>
      </div>
      <ConfirmationModal
        isOpen={deleteTarget !== null}
        text="Are you sure you want to delete this meeting? This action cannot be undone."
        onConfirm={handleDeleteConfirm}
        onCancel={() => setDeleteTarget(null)}
      />
    </div>
  );
}
