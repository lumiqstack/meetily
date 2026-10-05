export type BackgroundJobKind = 'import' | 'retranscription' | 'summary';

/** Store id for a summary job. Retranscription jobs use the bare meeting id,
 * so summary jobs are prefixed to allow both for one meeting at once. */
export function summaryJobId(meetingId: string): string {
  return `summary:${meetingId}`;
}

export type BackgroundJobStatus =
  | 'queued'
  | 'running'
  | 'cancelling'
  | 'completed'
  | 'error'
  | 'cancelled'
  | 'interrupted';

/**
 * A job a previous app process died under, as journaled by the backend
 * (`background_jobs` table, snake_case fields). Carries everything needed to
 * retry the job with its original settings.
 */
export interface InterruptedJobInfo {
  id: string;
  kind: BackgroundJobKind;
  title: string;
  source_path: string | null;
  /** URL import: the original link. Retry must go through the URL command. */
  source_url: string | null;
  /** URL import: "audio" or "transcript". */
  mode: string | null;
  folder_path: string | null;
  meeting_id: string | null;
  language: string | null;
  model: string | null;
  provider: string | null;
  /**
   * Gemini annotation options the job was started with. Retrying must reuse
   * them: silently re-running an authoritative pass without word timestamps
   * would replace a segmented transcript with one long block.
   */
  diarization: boolean;
  wordTimestamps: boolean;
  created_at: string;
}

export type CancelInvoker = (
  command: string,
  args: Record<string, string>
) => Promise<void>;

/** Generic Tauri-command invoker used by interrupted-job actions. */
export type CommandInvoker = (
  command: string,
  args: Record<string, unknown>
) => Promise<unknown>;

export interface BackgroundJob {
  id: string;
  kind: BackgroundJobKind;
  title: string;
  status: BackgroundJobStatus;
  progressPercentage: number;
  message: string;
  error: string | null;
  /** Present only on jobs recovered from a crashed session. */
  interrupted?: InterruptedJobInfo;
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
    case 'queued':
    case 'running':
    case 'cancelling':
    // An interrupted-job notice stays until the user retries or dismisses.
    case 'interrupted':
      return Infinity;
  }
}

/** Delay before dropping a finished job from the store: toast duration + 1s buffer. */
export function cleanupDelayMs(status: BackgroundJobStatus): number {
  return toastDurationMs(status) + 1000;
}

/**
 * Tracks whether an in-flow surface (currently the Home screen's job panel) is
 * on screen showing the same job cards the toasts would. While one is claimed
 * the floating toasts stay quiet, so a job is never drawn twice at once.
 */
export class InlineSurfaceRegistry {
  private claims = 0;
  private listeners = new Set<() => void>();

  subscribe(listener: () => void): () => void {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  }

  isVisible(): boolean {
    return this.claims > 0;
  }

  /** Claim on mount; call the returned release on unmount. */
  claim(): () => void {
    this.claims += 1;
    this.notify();

    let released = false;
    return () => {
      if (released) return;
      released = true;
      this.claims -= 1;
      this.notify();
    };
  }

  private notify(): void {
    this.listeners.forEach((listener) => listener());
  }
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

  /** Register an import waiting in a batch queue (not yet started). */
  registerQueued(importId: string, title: string): void {
    this.jobs.set(importId, {
      id: importId,
      kind: 'import',
      title,
      status: 'queued',
      progressPercentage: 0,
      message: 'Waiting…',
      error: null,
    });
    this.notify();
  }

  /** A queued import's turn arrived: mark it running. */
  promoteToActive(id: string): boolean {
    const job = this.jobs.get(id);
    if (!job || job.status !== 'queued') return false;

    job.status = 'running';
    job.message = '';
    this.notify();
    return true;
  }

  /**
   * Drop a job that never started — no backend call needed, unlike
   * cancelJob. Refuses to touch anything already running.
   */
  cancelQueued(id: string): boolean {
    const job = this.jobs.get(id);
    if (!job || job.status !== 'queued') return false;

    this.jobs.delete(id);
    this.notify();
    return true;
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

  registerSummary(meetingId: string, title?: string): void {
    this.jobs.set(summaryJobId(meetingId), {
      id: summaryJobId(meetingId),
      kind: 'summary',
      title: title || 'Summary generation',
      status: 'running',
      progressPercentage: 0,
      message: '',
      error: null,
    });
    this.notify();
  }

  registerInterrupted(info: InterruptedJobInfo): void {
    this.jobs.set(info.id, {
      id: info.id,
      kind: info.kind,
      title: info.title,
      status: 'interrupted',
      progressPercentage: 0,
      message: '',
      error: null,
      interrupted: info,
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
      } else if (job.kind === 'summary') {
        await invoke('api_cancel_summary', {
          meetingId: id.slice('summary:'.length),
        });
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

  /**
   * Restart an interrupted job with the settings journaled at its original
   * start, then drop the stale notice. The fresh job is registered as
   * running so the regular progress/completion listeners drive it.
   */
  async retryInterrupted(id: string, invoke: CommandInvoker): Promise<boolean> {
    const job = this.jobs.get(id);
    const info = job?.interrupted;
    if (!job || job.status !== 'interrupted' || !info) return false;

    let freshId: string;
    try {
      if (info.kind === 'import' && info.source_url) {
        // URL imports (audio or Teams-transcript mode) have no local source
        // file — the downloaded media was temporary. Re-run from the link.
        const started = (await invoke('start_import_from_url_command', {
          url: info.source_url,
          title: info.title,
          language: info.language,
          model: info.model,
          provider: info.provider,
          mode: info.mode,
        })) as { import_id: string };
        freshId = started.import_id;
      } else if (info.kind === 'import') {
        const started = (await invoke('start_import_audio_command', {
          sourcePath: info.source_path,
          title: info.title,
          language: info.language,
          model: info.model,
          provider: info.provider,
        })) as { import_id: string };
        freshId = started.import_id;
      } else {
        await invoke('start_retranscription_command', {
          meetingId: info.meeting_id,
          meetingFolderPath: info.folder_path,
          language: info.language,
          model: info.model,
          provider: info.provider,
          diarization: info.diarization ?? false,
        });
        freshId = info.meeting_id ?? id;
      }
    } catch {
      return false;
    }

    await invoke('dismiss_interrupted_job_command', { jobId: id });
    this.remove(id);
    if (info.kind === 'import') {
      this.registerImport(freshId, info.title);
    } else {
      this.registerRetranscription(freshId, info.title);
    }
    return true;
  }

  /**
   * Drop an interrupted-job notice: clears the backend journal row and
   * removes the job from the store.
   */
  async dismissInterrupted(id: string, invoke: CommandInvoker): Promise<boolean> {
    const job = this.jobs.get(id);
    if (!job || job.status !== 'interrupted') return false;

    try {
      await invoke('dismiss_interrupted_job_command', { jobId: id });
    } catch {
      return false;
    }
    this.remove(id);
    return true;
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
