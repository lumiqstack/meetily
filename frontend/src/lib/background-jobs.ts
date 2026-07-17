export type BackgroundJobKind = 'import' | 'retranscription';

export type BackgroundJobStatus =
  | 'running'
  | 'cancelling'
  | 'completed'
  | 'error'
  | 'cancelled';

export type CancelInvoker = (
  command: string,
  args: Record<string, string>
) => Promise<void>;

export interface BackgroundJob {
  id: string;
  kind: BackgroundJobKind;
  title: string;
  status: BackgroundJobStatus;
  progressPercentage: number;
  message: string;
  error: string | null;
}

/** How long a job toast stays on screen for a given status. */
export function toastDurationMs(status: BackgroundJobStatus): number {
  switch (status) {
    case 'completed':
      return 3000;
    case 'cancelled':
      return 5000;
    case 'error':
      return 10000;
    case 'running':
    case 'cancelling':
      return Infinity;
  }
}

/** Delay before dropping a finished job from the store: toast duration + 1s buffer. */
export function cleanupDelayMs(status: BackgroundJobStatus): number {
  return toastDurationMs(status) + 1000;
}

/**
 * Tracks remote import/retranscription jobs that continue after their dialog
 * closes. Pure state machine — no Tauri or React dependencies — so the toast
 * UI stays declarative glue.
 */
export class BackgroundJobStore {
  private jobs = new Map<string, BackgroundJob>();
  private listeners = new Set<() => void>();
  private snapshot: BackgroundJob[] | null = null;

  subscribe(listener: () => void): () => void {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  }

  getJobs(): BackgroundJob[] {
    if (this.snapshot === null) {
      this.snapshot = Array.from(this.jobs.values());
    }
    return this.snapshot;
  }

  private notify(): void {
    this.snapshot = null;
    this.listeners.forEach((listener) => listener());
  }

  has(id: string): boolean {
    return this.jobs.has(id);
  }

  registerImport(importId: string, title: string): void {
    this.jobs.set(importId, {
      id: importId,
      kind: 'import',
      title,
      status: 'running',
      progressPercentage: 0,
      message: '',
      error: null,
    });
    this.notify();
  }

  registerRetranscription(meetingId: string, title?: string): void {
    this.jobs.set(meetingId, {
      id: meetingId,
      kind: 'retranscription',
      title: title || 'Retranscription',
      status: 'running',
      progressPercentage: 0,
      message: '',
      error: null,
    });
    this.notify();
  }

  applyProgress(id: string, progressPercentage: number, message: string): boolean {
    const job = this.jobs.get(id);
    if (!job) return false;

    job.progressPercentage = progressPercentage;
    job.message = message;
    this.notify();
    return true;
  }

  applyComplete(id: string): boolean {
    const job = this.jobs.get(id);
    if (!job) return false;

    job.status = 'completed';
    job.progressPercentage = 100;
    this.notify();
    return true;
  }

  async cancelJob(id: string, invoke: CancelInvoker): Promise<boolean> {
    const job = this.jobs.get(id);
    if (!job) return false;

    const previousStatus = job.status;
    job.status = 'cancelling';
    this.notify();

    try {
      if (job.kind === 'import') {
        await invoke('cancel_import_command', { importId: id });
      } else {
        await invoke('cancel_retranscription_command', { meetingId: id });
      }
      return true;
    } catch {
      job.status = previousStatus;
      this.notify();
      return false;
    }
  }

  remove(id: string): void {
    if (this.jobs.delete(id)) {
      this.notify();
    }
  }

  applyError(id: string, error: string): boolean {
    const job = this.jobs.get(id);
    if (!job) return false;

    // The backend reports a user-requested cancellation as an error event
    // (e.g. import-error "Import cancelled"), so a job we are cancelling
    // ends as cancelled rather than failed.
    if (job.status === 'cancelling') {
      job.status = 'cancelled';
    } else {
      job.status = 'error';
      job.error = error;
    }
    this.notify();
    return true;
  }
}
