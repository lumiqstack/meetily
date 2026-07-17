'use client';

import React, { useCallback, useSyncExternalStore } from 'react';
import { toast } from 'sonner';
import {
  backgroundJobStore,
  BackgroundJobCard,
  cancelBackgroundJob,
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

  const handleCancel = useCallback((id: string) => {
    cancelBackgroundJob(id).then((cancelled) => {
      if (!cancelled) {
        toast.error('Failed to cancel job');
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
              className="max-w-none shadow-sm"
            />
          ))}
        </div>
      </div>
    </div>
  );
}
