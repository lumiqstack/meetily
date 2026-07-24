import { BackgroundJobStore } from './background-jobs';

/**
 * Drives a batch of file imports through the single-import backend command,
 * respecting the backend's concurrency rules instead of racing them:
 * local providers (whisper/parakeet) hold the on-device engine exclusively,
 * so local items run strictly one at a time; remote (openaiCompatible) jobs
 * are capped at 3 by the backend's semaphore, so at most 3 run here.
 *
 * The queue lives outside React (like backgroundJobStore) so a batch keeps
 * flowing after the import dialog closes. Progress/completion/error events
 * are routed into the store by BackgroundJobToastProvider as usual; the
 * queue subscribes to the store and starts the next item whenever an active
 * one reaches a terminal state. Queued-but-not-started items are lost if the
 * app dies — only started imports are journaled for crash recovery.
 */

const REMOTE_PROVIDER = 'openaiCompatible';
const MAX_ACTIVE_REMOTE = 3; // Mirrors MAX_CONCURRENT_REMOTE_JOBS in Rust.
const MAX_ACTIVE_LOCAL = 1; // Local engine is single-holder.

/** Statuses that free a queue slot. A job the toast already removed counts too. */
const TERMINAL: ReadonlySet<string> = new Set(['completed', 'error', 'cancelled']);

export interface BatchImportItem {
  path: string;
  title: string;
}

export interface BatchImportOptions {
  language?: string | null;
  model?: string | null;
  provider?: string | null;
  /** Delay before the single retry of a rejected start. Tests pass 0. */
  retryDelayMs?: number;
}

type Invoker = (command: string, args: Record<string, unknown>) => Promise<unknown>;

interface QueuedItem extends BatchImportItem {
  id: string;
  options: BatchImportOptions;
}

function defaultIdGenerator(): string {
  return `import-${crypto.randomUUID()}`;
}

function isRemote(options: BatchImportOptions): boolean {
  return options.provider === REMOTE_PROVIDER;
}

const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

export class ImportQueue {
  private queue: QueuedItem[] = [];
  private activeLocal = new Set<string>();
  private activeRemote = new Set<string>();
  private unsubscribe: (() => void) | null = null;

  constructor(
    private readonly store: BackgroundJobStore,
    private readonly invoke: Invoker,
    private readonly generateId: () => string = defaultIdGenerator,
  ) {}

  /** Number of items still waiting for a slot. */
  get pendingCount(): number {
    return this.queue.length;
  }

  /**
   * Add a batch. Every item is registered as a queued job immediately (so
   * the toast tray shows the whole batch), then items start as slots free
   * up. Returns the import ids in batch order.
   */
  enqueueBatch(items: BatchImportItem[], options: BatchImportOptions): string[] {
    const queued: QueuedItem[] = items.map((item) => ({
      ...item,
      id: this.generateId(),
      options,
    }));
    for (const item of queued) {
      this.store.registerQueued(item.id, item.title);
    }
    this.queue.push(...queued);
    this.ensureSubscribed();
    this.startNext();
    return queued.map((item) => item.id);
  }

  /** Cancel the active job(s) via the backend and drop everything queued. */
  async cancelRemaining(): Promise<void> {
    for (const item of this.queue.splice(0)) {
      this.store.cancelQueued(item.id);
    }
    const active = [...this.activeLocal, ...this.activeRemote];
    await Promise.all(
      active.map((id) =>
        this.store.cancelJob(id, (command, args) =>
          this.invoke(command, args).then(() => undefined),
        ),
      ),
    );
    // The backend confirms each cancel with an import-error event, which
    // reaches the store via the toast provider and frees the slot there.
  }

  /** Watch the store so terminal transitions free slots and pull the queue. */
  private ensureSubscribed(): void {
    if (this.unsubscribe) return;
    this.unsubscribe = this.store.subscribe(() => this.reapFinished());
  }

  private reapFinished(): void {
    let freed = false;
    for (const pool of [this.activeLocal, this.activeRemote]) {
      for (const id of [...pool]) {
        const job = this.store.getJobs().find((j) => j.id === id);
        if (!job || TERMINAL.has(job.status)) {
          pool.delete(id);
          freed = true;
        }
      }
    }
    if (freed) this.startNext();
  }

  /** Start queue-head items while their pool has room (strict FIFO). */
  private startNext(): void {
    while (this.queue.length > 0) {
      const head = this.queue[0];

      // Cancelled from the toast while waiting (store.cancelQueued): skip.
      if (!this.store.has(head.id)) {
        this.queue.shift();
        continue;
      }

      const pool = isRemote(head.options) ? this.activeRemote : this.activeLocal;
      const capacity = isRemote(head.options) ? MAX_ACTIVE_REMOTE : MAX_ACTIVE_LOCAL;
      if (pool.size >= capacity) return;

      this.queue.shift();
      pool.add(head.id);
      void this.startItem(head, pool);
    }
  }

  /**
   * Start one item; the backend is fail-fast on concurrency conflicts (e.g.
   * a user-initiated import raced us), so retry once after a pause, then
   * fail the item and let the rest of the batch continue.
   */
  private async startItem(item: QueuedItem, pool: Set<string>): Promise<void> {
    this.store.promoteToActive(item.id);
    const args = {
      importId: item.id,
      sourcePath: item.path,
      title: item.title,
      language: item.options.language ?? null,
      model: item.options.model ?? null,
      provider: item.options.provider ?? null,
    };

    try {
      await this.invoke('start_import_audio_command', args);
    } catch {
      await sleep(item.options.retryDelayMs ?? 2000);
      try {
        await this.invoke('start_import_audio_command', args);
      } catch (retryError) {
        this.store.applyError(item.id, String(retryError));
        pool.delete(item.id);
        this.startNext();
      }
    }
  }
}

/** Singleton used by the import dialog; tests construct their own. */
let sharedQueue: ImportQueue | null = null;

export function getSharedImportQueue(
  store: BackgroundJobStore,
  invoke: Invoker,
): ImportQueue {
  if (!sharedQueue) {
    sharedQueue = new ImportQueue(store, invoke);
  }
  return sharedQueue;
}
