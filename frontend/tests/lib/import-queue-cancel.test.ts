import { describe, expect, test } from "bun:test";

import { BackgroundJobStore } from "../../src/lib/background-jobs";
import { ImportQueue } from "../../src/lib/import-queue";

// Cancelling an import moves the job to 'cancelling' and only an import-error
// event from the backend ends it. The backend confirms every cancel with one
// (audio/import.rs), so the local (capacity-1) pool frees.

const flush = () => new Promise((resolve) => setTimeout(resolve, 0));

function urlItem(n: number) {
  return {
    url: `https://t-my.sharepoint.com/_layouts/15/stream.aspx?id=/personal/u/Recordings/rec-${n}.mp4`,
    title: `Rec ${n}`,
  };
}

function makeInvoker(
  started: { command: string; args: Record<string, unknown> }[],
  store: BackgroundJobStore,
) {
  return async (command: string, args: Record<string, unknown>) => {
    if (command.startsWith("cancel_")) {
      // The backend confirms the cancel with import-error, delivered to the
      // store the way the toast listener does.
      const importId = String(args.importId);
      setTimeout(() => store.applyError(importId, "Import cancelled"), 0);
      return undefined;
    }
    started.push({ command, args });
    return { import_id: args.importId, message: "Import started" };
  };
}

describe("cancelled imports release the local import slot", () => {
  test("a new import starts after the active import was cancelled and the cancel call succeeded", async () => {
    const store = new BackgroundJobStore();
    const started: { command: string; args: Record<string, unknown> }[] = [];
    const queue = new ImportQueue(store, makeInvoker(started, store), (() => {
      let n = 0;
      return () => `import-h8f2-${++n}`;
    })());

    queue.enqueueBatch([urlItem(1)], { provider: "whisper", retryDelayMs: 0 });
    await flush();
    expect(started.length).toBe(1);
    expect(started[0].command).toBe("start_import_from_url_command");

    await queue.cancelRemaining();
    await flush();

    queue.enqueueBatch([{ path: "C:/audio/after-cancel.mp3", title: "After cancel" }], {
      provider: "whisper",
      retryDelayMs: 0,
    });
    await flush();

    // Expected: the slot is free, so the new item starts.
    expect(started.length).toBe(2);
    expect(started[1].args.sourcePath).toBe("C:/audio/after-cancel.mp3");
  });

  test("a cancelled import leaves a terminal status once the backend accepted the cancel", async () => {
    const store = new BackgroundJobStore();
    const started: { command: string; args: Record<string, unknown> }[] = [];
    const queue = new ImportQueue(store, makeInvoker(started, store), (() => {
      let n = 0;
      return () => `import-h8f2b-${++n}`;
    })());

    const [id] = queue.enqueueBatch([urlItem(1)], { provider: "whisper", retryDelayMs: 0 });
    await flush();
    await queue.cancelRemaining();
    await flush();

    expect(store.getJobs().find((j) => j.id === id)?.status).toBe("cancelled");
  });
});

describe("import-error payload confirming a cancel", () => {
  test("the exact payload Rust emits for a cancelled URL import ends a cancelling job as cancelled", async () => {
    const store = new BackgroundJobStore();
    store.registerImport("import-h8f2-payload", "Rec payload");
    // Rust ImportError serializes as { import_id, error }.
    const rustPayload = { import_id: "import-h8f2-payload", error: "Import cancelled" };

    const pending = store.cancelJob("import-h8f2-payload", async () => undefined);
    expect(store.getJobs()[0].status).toBe("cancelling");

    store.applyError(rustPayload.import_id, rustPayload.error);
    await pending;

    const job = store.getJobs()[0];
    expect(job.status).toBe("cancelled");
    expect(job.error).toBeNull();
  });
});
