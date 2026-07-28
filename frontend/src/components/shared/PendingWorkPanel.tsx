'use client';

import React, { useCallback, useEffect, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { toast } from 'sonner';
import { Loader2, Pause, Play, Trash2, LogIn } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { ConfirmationModal } from '@/components/ConfirmationModel/confirmation-modal';
import { useSidebar } from '@/components/Sidebar/SidebarProvider';

type PipelineStage = 'transcribe' | 'summarize';

interface PendingSnapshotItem {
  meeting_id: string;
  title: string;
  stage: PipelineStage;
  created_at: string;
  attempts: number;
  last_error: string | null;
  suppressed: boolean;
  eligible: boolean;
}

interface CurrentItem {
  meeting_id: string;
  title: string;
  stage: string;
  started_at: string;
}

interface PipelineStatus {
  state: 'running' | 'paused' | 'auth_required';
  detail: string | null;
  enabled: boolean;
  current: CurrentItem | null;
  pending: PendingSnapshotItem[];
  last_scan_at: string | null;
}

const STAGE_LABEL: Record<PipelineStage, string> = {
  transcribe: 'Needs transcript',
  summarize: 'Needs summary',
};

const STAGE_BADGE: Record<PipelineStage, string> = {
  transcribe: 'bg-blue-50 text-blue-700',
  summarize: 'bg-amber-50 text-amber-700',
};

/**
 * Home-screen view of the automatic pipeline: what still needs transcribing
 * or summarizing, what it is working on right now, and why it is waiting.
 *
 * The processing itself lives in the Rust orchestrator
 * (`src-tauri/src/pipeline`), which keeps running with the window closed —
 * this panel only observes `pipeline-status` events and offers the manual
 * overrides: process a meeting now (skipping the idle wait), pause/resume,
 * sign in to SharePoint, and remove a meeting that keeps failing.
 */
export function PendingWorkPanel() {
  const { refetchMeetings } = useSidebar();
  const [status, setStatus] = useState<PipelineStatus | null>(null);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [deleteTarget, setDeleteTarget] = useState<PendingSnapshotItem | null>(null);
  const [busy, setBusy] = useState(false);

  const refresh = useCallback(async () => {
    try {
      setStatus(await invoke<PipelineStatus>('pipeline_get_status'));
    } catch (error) {
      console.error('Failed to read pipeline status:', error);
    }
  }, []);

  // The orchestrator pushes a status event on every state change, so the
  // panel never polls.
  useEffect(() => {
    void refresh();
    const unlisten = listen<PipelineStatus>('pipeline-status', (event) => {
      setStatus(event.payload);
    });
    return () => {
      void unlisten.then((fn) => fn());
    };
  }, [refresh]);

  // A completed meeting leaves the pending list; keep the sidebar in step.
  useEffect(() => {
    if (!status) return;
    setSelected((prev) => {
      const alive = new Set(status.pending.map((item) => item.meeting_id));
      const next = new Set([...prev].filter((id) => alive.has(id)));
      return next.size === prev.size ? prev : next;
    });
  }, [status]);

  const items = status?.pending ?? [];
  const current = status?.current ?? null;

  const toggleItem = useCallback((meetingId: string) => {
    setSelected((prev) => {
      const next = new Set(prev);
      next.has(meetingId) ? next.delete(meetingId) : next.add(meetingId);
      return next;
    });
  }, []);

  const allSelected = items.length > 0 && items.every((i) => selected.has(i.meeting_id));
  const toggleAll = useCallback(() => {
    setSelected(allSelected ? new Set() : new Set(items.map((i) => i.meeting_id)));
  }, [allSelected, items]);

  const handleProcessNow = useCallback(async () => {
    const meetingIds = [...selected];
    if (!meetingIds.length) return;
    setBusy(true);
    try {
      await invoke('pipeline_process_now', { meetingIds });
      setSelected(new Set());
      toast.success(
        meetingIds.length === 1
          ? 'Queued for processing now'
          : `Queued ${meetingIds.length} meetings for processing now`
      );
    } catch (error) {
      toast.error('Could not start processing', {
        description: error instanceof Error ? error.message : String(error),
      });
    } finally {
      setBusy(false);
    }
  }, [selected]);

  const handlePauseResume = useCallback(async () => {
    if (!status) return;
    setBusy(true);
    try {
      await invoke(status.state === 'paused' ? 'pipeline_resume' : 'pipeline_pause');
    } catch (error) {
      toast.error('Could not change the pipeline state', {
        description: error instanceof Error ? error.message : String(error),
      });
    } finally {
      setBusy(false);
    }
  }, [status]);

  const handleSignIn = useCallback(async () => {
    setBusy(true);
    try {
      await invoke('pipeline_sign_in_to_sharepoint');
      toast.success('Signed in to SharePoint — importing resumed');
    } catch (error) {
      toast.error('SharePoint sign-in failed', {
        description: error instanceof Error ? error.message : String(error),
      });
    } finally {
      setBusy(false);
    }
  }, []);

  const handleDeleteConfirm = useCallback(async () => {
    if (!deleteTarget) return;
    const meetingId = deleteTarget.meeting_id;
    setDeleteTarget(null);
    try {
      await invoke('api_delete_meeting', { meetingId });
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
  }, [deleteTarget, refresh, refetchMeetings]);

  // Nothing outstanding and nothing to report: stay out of the way.
  if (!status || (items.length === 0 && !current && status.state === 'running')) {
    return null;
  }

  const stateBanner =
    status.state === 'auth_required'
      ? status.detail ?? 'SharePoint sign-in needed'
      : status.state === 'paused'
        ? 'Pipeline paused — meetings stay in the list until you resume'
        : null;

  return (
    <div className="flex justify-center px-4 pt-4">
      <div className="w-2/3 max-w-[750px]">
        <p className="text-xs font-medium text-gray-500 uppercase tracking-wide mb-2">
          Pending work
        </p>
        <div className="bg-white rounded-lg shadow-sm border border-gray-200 p-3">
          {stateBanner && (
            <div className="flex items-center justify-between gap-2 mb-2 px-2 py-1.5 rounded bg-amber-50 text-[12px] text-amber-800">
              <span className="truncate">{stateBanner}</span>
              {status.state === 'auth_required' && (
                <Button size="sm" variant="outline" onClick={handleSignIn} disabled={busy}>
                  <LogIn className="w-3.5 h-3.5" />
                  Sign in
                </Button>
              )}
            </div>
          )}

          {current && (
            <div className="flex items-center gap-2 mb-2 px-2 py-1.5 rounded bg-gray-50 text-[12px] text-gray-700">
              <Loader2 className="w-3.5 h-3.5 animate-spin flex-shrink-0" />
              <span className="truncate">
                {current.stage === 'transcribe' ? 'Transcribing' : 'Summarizing'}{' '}
                <span className="font-medium">{current.title}</span>
              </span>
            </div>
          )}

          {items.length > 1 && (
            <label className="flex items-center gap-2 pb-2 mb-1 border-b border-gray-100 text-xs text-gray-500 cursor-pointer select-none">
              <input
                type="checkbox"
                checked={allSelected}
                onChange={toggleAll}
                className="h-4 w-4 rounded border-gray-300 accent-gray-900"
              />
              Select all
            </label>
          )}

          <div className="space-y-1">
            {items.map((item) => {
              const isCurrent = current?.meeting_id === item.meeting_id;
              return (
                <label
                  key={item.meeting_id}
                  className="flex items-center gap-2 py-1.5 cursor-pointer select-none"
                >
                  <input
                    type="checkbox"
                    checked={selected.has(item.meeting_id)}
                    onChange={() => toggleItem(item.meeting_id)}
                    disabled={isCurrent}
                    className="h-4 w-4 rounded border-gray-300 accent-gray-900"
                  />
                  <span className="flex-1 min-w-0">
                    <span className="block text-sm text-gray-900 truncate">{item.title}</span>
                    {item.last_error && (
                      <span
                        className="block text-[11px] text-red-600 truncate"
                        title={item.last_error}
                      >
                        {item.last_error}
                      </span>
                    )}
                  </span>

                  {item.suppressed ? (
                    <span className="flex-shrink-0 text-[11px] font-medium px-2 py-0.5 rounded-full bg-red-50 text-red-700">
                      Failed
                    </span>
                  ) : item.last_error ? (
                    <span
                      className="flex-shrink-0 text-[11px] font-medium px-2 py-0.5 rounded-full bg-orange-50 text-orange-700"
                      title={`Attempt ${item.attempts} failed; will retry`}
                    >
                      Retrying
                    </span>
                  ) : (
                    <span
                      className={`flex-shrink-0 text-[11px] font-medium px-2 py-0.5 rounded-full ${STAGE_BADGE[item.stage]}`}
                    >
                      {STAGE_LABEL[item.stage]}
                    </span>
                  )}

                  <span className="flex-shrink-0 text-xs text-gray-400">
                    {new Date(item.created_at).toLocaleDateString()}
                  </span>

                  {(item.suppressed || item.last_error) && (
                    <button
                      type="button"
                      onClick={(e) => {
                        e.preventDefault();
                        e.stopPropagation();
                        setDeleteTarget(item);
                      }}
                      disabled={isCurrent}
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

          <div className="flex items-center justify-between pt-2 mt-1 border-t border-gray-100">
            <Button size="sm" variant="ghost" onClick={handlePauseResume} disabled={busy}>
              {status.state === 'paused' ? (
                <>
                  <Play className="w-4 h-4" />
                  Resume
                </>
              ) : (
                <>
                  <Pause className="w-4 h-4" />
                  Pause
                </>
              )}
            </Button>
            <Button size="sm" onClick={handleProcessNow} disabled={selected.size === 0 || busy}>
              {`Process now${selected.size > 0 ? ` (${selected.size})` : ''}`}
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
