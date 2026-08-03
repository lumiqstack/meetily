'use client';

import React, { useCallback, useEffect, useRef, useSyncExternalStore } from 'react';
import { listen, UnlistenFn } from '@tauri-apps/api/event';
import { invoke } from '@tauri-apps/api/core';
import { toast } from 'sonner';
import { Check, FileAudio, FileText, X } from 'lucide-react';
import {
  BackgroundJob,
  BackgroundJobStore,
  InlineSurfaceRegistry,
  InterruptedJobInfo,
  cleanupDelayMs,
  toastDurationMs,
} from '@/lib/background-jobs';
import { useSidebar } from '@/components/Sidebar/SidebarProvider';
import { cn } from '@/lib/utils';
import Analytics from '@/lib/analytics';
import { applyPinnedSummaryLanguageToMeeting } from '@/lib/summary-language-preferences';
import type { ImportError, ImportProgress, ImportResult } from '@/hooks/useImportAudio';

// Single app-wide registry of remote jobs that keep running after their
// dialog closes. Dialogs announce jobs via the window custom events below;
// everything else (progress, completion, cancel) flows through this store.
export const backgroundJobStore = new BackgroundJobStore();

/** Claimed by in-flow job surfaces so the toasts can stand down. */
export const inlineJobSurfaces = new InlineSurfaceRegistry();

interface BackgroundImportStartedDetail {
  importId: string;
  title: string;
}

interface BackgroundRetranscriptionStartedDetail {
  meetingId: string;
  title?: string;
}

interface RetranscriptionProgress {
  meeting_id: string;
  stage: string;
  progress_percentage: number;
  message: string;
}

interface RetranscriptionResult {
  meeting_id: string;
  segments_count: number;
  duration_seconds: number;
  language: string | null;
}

interface RetranscriptionError {
  meeting_id: string;
  error: string;
}

function jobKindLabel(kind: BackgroundJob['kind']): string {
  switch (kind) {
    case 'import':
      return 'Import';
    case 'summary':
      return 'Summary generation';
    default:
      return 'Retranscription';
  }
}

/** Cancel a background job through the store, invoking the matching Tauri command. */
export function cancelBackgroundJob(id: string): Promise<boolean> {
  return backgroundJobStore.cancelJob(id, async (command, args) => {
    await invoke(command, args);
  });
}

export function BackgroundJobCard({
  job,
  onCancel,
  onRetry,
  onDismiss,
  className,
}: {
  job: BackgroundJob;
  onCancel: (id: string) => void;
  onRetry?: (id: string) => void;
  onDismiss?: (id: string) => void;
  className?: string;
}) {
  const isActive = job.status === 'running' || job.status === 'cancelling';
  const isQueued = job.status === 'queued';
  const isComplete = job.status === 'completed';
  const hasError = job.status === 'error';
  const isInterrupted = job.status === 'interrupted';

  return (
    <div
      className={cn(
        'flex items-center gap-3 w-full max-w-sm bg-white rounded-lg shadow-lg border border-gray-200 p-3 relative',
        className
      )}
    >
      {/* Icon */}
      <div
        className={`flex-shrink-0 w-8 h-8 rounded-full flex items-center justify-center ${
          isComplete ? 'bg-green-100' : hasError ? 'bg-red-100' : 'bg-gray-100'
        }`}
      >
        {isComplete ? (
          <Check className="w-4 h-4 text-green-600" />
        ) : hasError || job.status === 'cancelled' ? (
          <X className={`w-4 h-4 ${hasError ? 'text-red-600' : 'text-gray-600'}`} />
        ) : job.kind === 'summary' ? (
          <FileText className="w-4 h-4 text-gray-600" />
        ) : (
          <FileAudio className="w-4 h-4 text-gray-600" />
        )}
      </div>

      {/* Content */}
      <div className="flex-1 min-w-0">
        <div className="flex items-center justify-between gap-2 mb-1">
          <p className="text-sm font-medium text-gray-900 truncate">{job.title}</p>
          {(isActive || isQueued) && (
            <button
              type="button"
              onClick={() =>
                isQueued ? backgroundJobStore.cancelQueued(job.id) : onCancel(job.id)
              }
              disabled={job.status === 'cancelling'}
              className="flex-shrink-0 text-xs text-gray-500 hover:text-red-600 disabled:opacity-50 disabled:hover:text-gray-500"
              aria-label={`Cancel ${job.kind}`}
            >
              {job.status === 'cancelling' ? 'Cancelling...' : 'Cancel'}
            </button>
          )}
        </div>

        {isInterrupted ? (
          <div className="flex items-center justify-between gap-2">
            <p className="text-xs text-amber-600">
              {`${jobKindLabel(job.kind)} interrupted by app restart`}
            </p>
            <div className="flex gap-3 flex-shrink-0">
              <button
                type="button"
                onClick={() => onRetry?.(job.id)}
                className="text-xs font-medium text-gray-900 hover:text-blue-600"
                aria-label={`Retry ${job.kind}`}
              >
                Retry
              </button>
              <button
                type="button"
                onClick={() => onDismiss?.(job.id)}
                className="text-xs text-gray-500 hover:text-red-600"
                aria-label={`Dismiss interrupted ${job.kind}`}
              >
                Dismiss
              </button>
            </div>
          </div>
        ) : hasError ? (
          <p className="text-xs text-red-600">
            {job.error || `${jobKindLabel(job.kind)} failed`}
          </p>
        ) : isComplete ? (
          <p className="text-xs text-green-600">{job.message || 'Complete'}</p>
        ) : job.status === 'cancelled' ? (
          <p className="text-xs text-gray-600">Cancelled</p>
        ) : isQueued ? (
          <p className="text-xs text-gray-500">Queued — waiting for a free slot…</p>
        ) : (
          <>
            {/* Progress bar */}
            <div className="w-full h-1.5 bg-gray-200 rounded-full overflow-hidden mb-1.5">
              <div
                className="h-full bg-gray-900 rounded-full transition-all duration-300"
                style={{ width: `${job.progressPercentage}%` }}
              />
            </div>

            {/* Progress text */}
            <div className="flex items-center justify-between text-xs text-gray-500">
              <span className="truncate">{job.message || 'Processing...'}</span>
              <span className="text-gray-900 font-medium flex-shrink-0">
                {Math.round(job.progressPercentage)}%
              </span>
            </div>
          </>
        )}
      </div>
    </div>
  );
}

/**
 * App-level provider that turns background remote jobs (imports and
 * retranscriptions that outlive their dialog) into per-job progress toasts
 * with a cancel button, and performs the completion side effects that used
 * to live in BackgroundImportListener.
 */
export function BackgroundJobToastProvider() {
  const { refetchMeetings } = useSidebar();
  const jobs = useSyncExternalStore(
    useCallback((listener) => backgroundJobStore.subscribe(listener), []),
    () => backgroundJobStore.getJobs(),
    () => backgroundJobStore.getJobs()
  );

  // Jobs a previous app process died under (crash, force-quit) are journaled
  // by the backend and reconciled at startup; surface them once on mount so
  // the user can retry or dismiss each one.
  useEffect(() => {
    let cancelled = false;
    invoke<InterruptedJobInfo[]>('list_interrupted_jobs_command')
      .then((interrupted) => {
        if (cancelled) return;
        interrupted.forEach((info) => {
          if (!backgroundJobStore.has(info.id)) {
            backgroundJobStore.registerInterrupted(info);
          }
        });
      })
      .catch((error) => {
        console.warn('Failed to query interrupted background jobs:', error);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  // Dialogs announce jobs that continue in the background.
  useEffect(() => {
    const handleImportStarted = (event: Event) => {
      const detail = (event as CustomEvent<BackgroundImportStartedDetail>).detail;
      if (!detail?.importId) return;
      backgroundJobStore.registerImport(detail.importId, detail.title || 'Imported Audio');
    };

    const handleRetranscriptionStarted = (event: Event) => {
      const detail = (event as CustomEvent<BackgroundRetranscriptionStartedDetail>).detail;
      if (!detail?.meetingId) return;
      backgroundJobStore.registerRetranscription(detail.meetingId, detail.title);
    };

    window.addEventListener('meetily-background-import-started', handleImportStarted);
    window.addEventListener('meetily-background-retranscription-started', handleRetranscriptionStarted);

    return () => {
      window.removeEventListener('meetily-background-import-started', handleImportStarted);
      window.removeEventListener('meetily-background-retranscription-started', handleRetranscriptionStarted);
    };
  }, []);

  // Backend events drive the store; the store decides whether the job is
  // one of ours (foreground dialog jobs are ignored).
  useEffect(() => {
    const unlisteners: UnlistenFn[] = [];
    const cleanedUpRef = { current: false };

    const register = (unlisten: UnlistenFn): boolean => {
      if (cleanedUpRef.current) {
        unlisten();
        unlisteners.forEach((fn) => fn());
        return false;
      }
      unlisteners.push(unlisten);
      return true;
    };

    const setupListeners = async () => {
      const unlistenImportProgress = await listen<ImportProgress>('import-progress', (event) => {
        backgroundJobStore.applyProgress(
          event.payload.import_id,
          event.payload.progress_percentage,
          event.payload.message
        );
      });
      if (!register(unlistenImportProgress)) return;

      const unlistenImportComplete = await listen<ImportResult>('import-complete', async (event) => {
        const summary = `${event.payload.segments_count} segments created.`;
        if (!backgroundJobStore.applyProgress(event.payload.import_id, 100, summary)) return;
        backgroundJobStore.applyComplete(event.payload.import_id);

        await Analytics.track('import_audio_completed', {
          success: 'true',
          duration_seconds: event.payload.duration_seconds.toString(),
          segments_count: event.payload.segments_count.toString(),
        });

        try {
          await applyPinnedSummaryLanguageToMeeting(event.payload.meeting_id);
        } catch (error) {
          console.warn('Failed to apply pinned summary language to background import:', error);
          toast.warning('Could not apply default summary language', {
            description: 'The imported meeting was saved, but the default summary language was not applied.',
          });
        }

        await refetchMeetings();
      });
      if (!register(unlistenImportComplete)) return;

      const unlistenImportError = await listen<ImportError>('import-error', async (event) => {
        if (!backgroundJobStore.applyError(event.payload.import_id, event.payload.error)) return;

        // A user-requested cancel also arrives here; only real failures are errors.
        const job = backgroundJobStore
          .getJobs()
          .find((j) => j.id === event.payload.import_id);
        if (job?.status === 'error') {
          await Analytics.trackError('import_audio_failed', event.payload.error);
        }
      });
      if (!register(unlistenImportError)) return;

      const unlistenRetranscriptionProgress = await listen<RetranscriptionProgress>(
        'retranscription-progress',
        (event) => {
          backgroundJobStore.applyProgress(
            event.payload.meeting_id,
            event.payload.progress_percentage,
            event.payload.message
          );
        }
      );
      if (!register(unlistenRetranscriptionProgress)) return;

      const unlistenRetranscriptionComplete = await listen<RetranscriptionResult>(
        'retranscription-complete',
        async (event) => {
          const summary = `${event.payload.segments_count} segments created.`;
          if (!backgroundJobStore.applyProgress(event.payload.meeting_id, 100, summary)) return;
          backgroundJobStore.applyComplete(event.payload.meeting_id);

          await Analytics.track('enhance_transcript_completed', {
            success: 'true',
            duration_seconds: event.payload.duration_seconds.toString(),
            segments_count: event.payload.segments_count.toString(),
          });

          window.dispatchEvent(new CustomEvent('meetily-background-retranscription-complete', {
            detail: {
              meetingId: event.payload.meeting_id,
            },
          }));
        }
      );
      if (!register(unlistenRetranscriptionComplete)) return;

      const unlistenRetranscriptionError = await listen<RetranscriptionError>(
        'retranscription-error',
        async (event) => {
          if (!backgroundJobStore.applyError(event.payload.meeting_id, event.payload.error)) return;

          const job = backgroundJobStore
            .getJobs()
            .find((j) => j.id === event.payload.meeting_id);
          if (job?.status === 'error') {
            await Analytics.trackError('enhance_transcript_failed', event.payload.error);
          }
        }
      );
      if (!register(unlistenRetranscriptionError)) return;

      // The Rust core auto-exports completed summaries to Obsidian when the
      // user has it enabled; surface each written note.
      const unlistenObsidianExport = await listen<{ meeting_id: string; relative_path: string }>(
        'obsidian-export-complete',
        (event) => {
          toast.success('Saved to Obsidian', {
            description: event.payload.relative_path,
          });
        }
      );
      if (!register(unlistenObsidianExport)) return;
    };

    setupListeners();

    return () => {
      cleanedUpRef.current = true;
      unlisteners.forEach((unlisten) => unlisten());
    };
  }, [refetchMeetings]);

  const handleCancel = useCallback((id: string) => {
    cancelBackgroundJob(id).then((cancelled) => {
      if (!cancelled) {
        toast.error('Failed to cancel job');
      }
    });
  }, []);

  const handleRetry = useCallback((id: string) => {
    backgroundJobStore
      .retryInterrupted(id, (command, args) => invoke(command, args))
      .then((retried) => {
        if (retried) {
          // The notice toast has Infinity duration and (for imports) the
          // fresh job runs under a new id, so drop the old toast explicitly.
          toast.dismiss(`bg-job-${id}`);
        } else {
          toast.error('Failed to restart the interrupted job');
        }
      });
  }, []);

  const handleDismiss = useCallback((id: string) => {
    backgroundJobStore
      .dismissInterrupted(id, (command, args) => invoke(command, args))
      .then((dismissed) => {
        if (dismissed) {
          toast.dismiss(`bg-job-${id}`);
        } else {
          toast.error('Failed to dismiss the interrupted job');
        }
      });
  }, []);

  // Drop finished jobs from the store after their toast has auto-dismissed.
  const scheduledRemovalsRef = useRef<Set<string>>(new Set());
  useEffect(() => {
    jobs.forEach((job) => {
      const isTerminal =
        job.status === 'completed' || job.status === 'error' || job.status === 'cancelled';
      if (!isTerminal || scheduledRemovalsRef.current.has(job.id)) return;

      scheduledRemovalsRef.current.add(job.id);
      setTimeout(() => {
        scheduledRemovalsRef.current.delete(job.id);
        backgroundJobStore.remove(job.id);
      }, cleanupDelayMs(job.status));
    });
  }, [jobs]);

  // Render one toast per job; re-calling toast.custom with the same id updates
  // it in place. Skipped entirely while an in-flow surface (the Home screen
  // panel) is already showing the same cards, so no job appears twice.
  const inlineSurfaceVisible = useSyncExternalStore(
    useCallback((listener) => inlineJobSurfaces.subscribe(listener), []),
    () => inlineJobSurfaces.isVisible(),
    () => false
  );

  useEffect(() => {
    if (inlineSurfaceVisible) {
      jobs.forEach((job) => toast.dismiss(`bg-job-${job.id}`));
      return;
    }

    jobs.forEach((job) => {
      toast.custom(
        () => (
          <BackgroundJobCard
            job={job}
            onCancel={handleCancel}
            onRetry={handleRetry}
            onDismiss={handleDismiss}
          />
        ),
        {
          id: `bg-job-${job.id}`,
          duration: toastDurationMs(job.status),
        }
      );
    });
  }, [jobs, inlineSurfaceVisible, handleCancel, handleRetry, handleDismiss]);

  return null;
}
