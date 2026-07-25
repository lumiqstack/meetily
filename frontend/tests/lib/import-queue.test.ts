import { describe, expect, test } from "bun:test";

import { BackgroundJobStore } from "../../src/lib/background-jobs";
import { ImportQueue } from "../../src/lib/import-queue";

/** Deterministic id generator so tests can address jobs. */
function idsFrom(prefix: string): () => string {
  let n = 0;
  return () => `${prefix}-${++n}`;
}

interface StartedCall {
  command: string;
  args: Record<string, unknown>;
}

/** Fake invoker recording start calls; per-call behavior via `failures`. */
function fakeInvoke(started: StartedCall[], failures: { remaining: number } = { remaining: 0 }) {
  return async (command: string, args: Record<string, unknown>) => {
    if (failures.remaining > 0) {
      failures.remaining -= 1;
      throw new Error("Cannot start a local import: engine busy");
    }
    started.push({ command, args });
    return { import_id: args.importId, message: "Import started" };
  };
}

function items(n: number): { path: string; title: string }[] {
  return Array.from({ length: n }, (_, i) => ({
    path: `C:/audio/file-${i}.mp3`,
    title: `File ${i}`,
  }));
}

const flush = () => new Promise((resolve) => setTimeout(resolve, 0));

describe("BackgroundJobStore queued jobs", () => {
  test("registerQueued adds a queued import that promoteToActive starts", () => {
    const store = new BackgroundJobStore();
    store.registerQueued("import-q1", "Queued file");

    let job = store.getJobs().find((j) => j.id === "import-q1");
    expect(job?.status).toBe("queued");
    expect(job?.kind).toBe("import");

    store.promoteToActive("import-q1");
    job = store.getJobs().find((j) => j.id === "import-q1");
    expect(job?.status).toBe("running");
  });

  test("cancelQueued removes a queued job without touching running ones", () => {
    const store = new BackgroundJobStore();
    store.registerQueued("import-q1", "Queued");
    store.registerImport("import-r1", "Running");

    expect(store.cancelQueued("import-q1")).toBe(true);
    expect(store.cancelQueued("import-r1")).toBe(false);
    expect(store.has("import-q1")).toBe(false);
    expect(store.has("import-r1")).toBe(true);
  });
});

describe("ImportQueue local provider (exclusive engine)", () => {
  test("runs strictly one at a time, in order", async () => {
    const store = new BackgroundJobStore();
    const started: StartedCall[] = [];
    const queue = new ImportQueue(store, fakeInvoke(started), idsFrom("import-loc"));

    queue.enqueueBatch(items(3), { provider: "whisper", retryDelayMs: 0 });
    await flush();

    expect(started.length).toBe(1);
    expect(started[0].args.sourcePath).toBe("C:/audio/file-0.mp3");

    store.applyComplete("import-loc-1");
    await flush();
    expect(started.length).toBe(2);
    expect(started[1].args.sourcePath).toBe("C:/audio/file-1.mp3");

    store.applyComplete("import-loc-2");
    await flush();
    expect(started.length).toBe(3);
  });

  test("an item that errors does not block the rest", async () => {
    const store = new BackgroundJobStore();
    const started: StartedCall[] = [];
    const queue = new ImportQueue(store, fakeInvoke(started), idsFrom("import-err"));

    queue.enqueueBatch(items(2), { provider: "whisper", retryDelayMs: 0 });
    await flush();
    expect(started.length).toBe(1);

    store.applyError("import-err-1", "decode failed");
    await flush();
    expect(started.length).toBe(2);
  });
});

describe("ImportQueue remote provider (capped at 3)", () => {
  test("starts three wide and refills as jobs finish", async () => {
    const store = new BackgroundJobStore();
    const started: StartedCall[] = [];
    const queue = new ImportQueue(store, fakeInvoke(started), idsFrom("import-rem"));

    queue.enqueueBatch(items(5), { provider: "openaiCompatible", retryDelayMs: 0 });
    await flush();
    expect(started.length).toBe(3);

    store.applyComplete("import-rem-2");
    await flush();
    expect(started.length).toBe(4);

    store.applyComplete("import-rem-1");
    store.applyComplete("import-rem-3");
    await flush();
    expect(started.length).toBe(5);
  });
});

describe("ImportQueue start failures", () => {
  test("retries a rejected start once, then fails the item and continues", async () => {
    const store = new BackgroundJobStore();
    const started: StartedCall[] = [];
    // Two consecutive rejections: initial + the single retry both fail.
    const queue = new ImportQueue(
      store,
      fakeInvoke(started, { remaining: 2 }),
      idsFrom("import-rej"),
    );

    queue.enqueueBatch(items(2), { provider: "whisper", retryDelayMs: 0 });
    await flush();
    await flush();

    const failed = store.getJobs().find((j) => j.id === "import-rej-1");
    expect(failed?.status).toBe("error");
    // The second item started despite the first one failing to start.
    expect(started.length).toBe(1);
    expect(started[0].args.sourcePath).toBe("C:/audio/file-1.mp3");
  });

  test("a rejection followed by a successful retry proceeds normally", async () => {
    const store = new BackgroundJobStore();
    const started: StartedCall[] = [];
    const queue = new ImportQueue(
      store,
      fakeInvoke(started, { remaining: 1 }),
      idsFrom("import-retry"),
    );

    queue.enqueueBatch(items(1), { provider: "whisper", retryDelayMs: 0 });
    await flush();
    await flush();

    expect(started.length).toBe(1);
    const job = store.getJobs().find((j) => j.id === "import-retry-1");
    expect(job?.status).toBe("running");
  });
});

describe("ImportQueue URL items (SharePoint sync)", () => {
  function urlItems(n: number): { url: string; title: string }[] {
    return Array.from({ length: n }, (_, i) => ({
      url: `https://t-my.sharepoint.com/_layouts/15/stream.aspx?id=/personal/u/Recordings/rec-${i}.mp4`,
      title: `Rec ${i}`,
    }));
  }

  test("uses the URL import command with audio mode", async () => {
    const store = new BackgroundJobStore();
    const started: StartedCall[] = [];
    const queue = new ImportQueue(store, fakeInvoke(started), idsFrom("import-url"));

    queue.enqueueBatch(urlItems(1), { provider: "whisper", retryDelayMs: 0 });
    await flush();

    expect(started.length).toBe(1);
    expect(started[0].command).toBe("start_import_from_url_command");
    expect(started[0].args.url).toContain("stream.aspx");
    expect(started[0].args.mode).toBe("audio");
    expect(started[0].args.sourcePath).toBeUndefined();
  });

  test("URL items run one at a time even on the remote provider (shared auth webview)", async () => {
    const store = new BackgroundJobStore();
    const started: StartedCall[] = [];
    const queue = new ImportQueue(store, fakeInvoke(started), idsFrom("import-urlseq"));

    queue.enqueueBatch(urlItems(3), { provider: "openaiCompatible", retryDelayMs: 0 });
    await flush();
    expect(started.length).toBe(1);

    store.applyComplete("import-urlseq-1");
    await flush();
    expect(started.length).toBe(2);
  });

  test("onItemCompleted fires for completed items only", async () => {
    const store = new BackgroundJobStore();
    const started: StartedCall[] = [];
    const completed: string[] = [];
    const queue = new ImportQueue(store, fakeInvoke(started), idsFrom("import-mark"));

    queue.enqueueBatch(urlItems(2), {
      provider: "whisper",
      retryDelayMs: 0,
      onItemCompleted: (item) => completed.push(item.url ?? ""),
    });
    await flush();

    store.applyComplete("import-mark-1");
    await flush();
    store.applyError("import-mark-2", "download failed");
    await flush();

    expect(completed.length).toBe(1);
    expect(completed[0]).toContain("rec-0.mp4");
  });
});

describe("ImportQueue cancellation", () => {
  test("an item cancelled from the store (toast) is skipped, not started", async () => {
    const store = new BackgroundJobStore();
    const started: StartedCall[] = [];
    const queue = new ImportQueue(store, fakeInvoke(started), idsFrom("import-skip"));

    queue.enqueueBatch(items(3), { provider: "whisper", retryDelayMs: 0 });
    await flush();
    expect(started.length).toBe(1);

    // User clicks the queued toast's cancel: only the store knows.
    store.cancelQueued("import-skip-2");

    store.applyComplete("import-skip-1");
    await flush();

    // Item 2 was skipped; item 3 started in its place.
    expect(started.length).toBe(2);
    expect(started[1].args.sourcePath).toBe("C:/audio/file-2.mp3");
  });

  test("cancelRemaining drops queued items and cancels the active one", async () => {
    const store = new BackgroundJobStore();
    const started: StartedCall[] = [];
    const cancelCalls: StartedCall[] = [];
    const invoke = async (command: string, args: Record<string, unknown>) => {
      if (command.startsWith("cancel_")) {
        cancelCalls.push({ command, args });
        return undefined;
      }
      started.push({ command, args });
      return { import_id: args.importId };
    };
    const queue = new ImportQueue(store, invoke, idsFrom("import-can"));

    queue.enqueueBatch(items(3), { provider: "whisper", retryDelayMs: 0 });
    await flush();
    expect(started.length).toBe(1);

    await queue.cancelRemaining();
    await flush();

    // Active job got a backend cancel; queued ones just vanished.
    expect(cancelCalls.length).toBe(1);
    expect(cancelCalls[0].args.importId).toBe("import-can-1");
    expect(store.has("import-can-2")).toBe(false);
    expect(store.has("import-can-3")).toBe(false);

    // Backend confirms the cancellation via an error event; nothing new starts.
    store.applyError("import-can-1", "Import cancelled");
    await flush();
    expect(started.length).toBe(1);
  });
});
