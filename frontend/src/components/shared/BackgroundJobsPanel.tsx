'use client';

import React, { useCallback, useEffect, useSyncExternalStore } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { toast } from 'sonner';
import {
  backgroundJobStore,
  BackgroundJobCard,
  cancelBackgroundJob,
  inlineJobSurfaces,
} from './BackgroundJobToast';

/**
 * Home-screen surface for background remote jobs (imports and
 * retranscriptions). Shows the same per-job cards as the toasts, but in-flow
 * on the Home page so running jobs stay discoverable after their dialog
 * closes. Renders nothing when no background job is active or recently
 * finished.
 */
export function BackgroundJobsPanel() {
  const jobs = useSyncExternalStore(
    useCallback((listener: () => void) => backgroundJobStore.subscribe(listener), []),
    () => backgroundJobStore.getJobs(),
    () => backgroundJobStore.getJobs()
  );

  // While this panel is on screen it owns the job cards; the floating toasts
  // stand down so each job is drawn once.
  useEffect(() => inlineJobSurfaces.claim(), []);

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
        if (!retried) {
          toast.error('Failed to restart the interrupted job');
        }
      });
  }, []);

  const handleDismiss = useCallback((id: string) => {
    backgroundJobStore
      .dismissInterrupted(id, (command, args) => invoke(command, args))
      .then((dismissed) => {
        if (!dismissed) {
          toast.error('Failed to dismiss the interrupted job');
        }
      });
  }, []);

  if (jobs.length === 0) {
    return null;
  }

  return (
    <div className="flex justify-center px-4 pt-4">
      <div className="w-2/3 max-w-[750px]">
        <p className="text-xs font-medium text-gray-500 uppercase tracking-wide mb-2">
          Background transcriptions
        </p>
        <div className="space-y-2">
          {jobs.map((job) => (
            <BackgroundJobCard
              key={job.id}
              job={job}
              onCancel={handleCancel}
              onRetry={handleRetry}
              onDismiss={handleDismiss}
              className="max-w-none shadow-sm"
            />
          ))}
        </div>
      </div>
    </div>
  );
}
