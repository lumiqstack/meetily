import { describe, expect, test } from "bun:test";

import { BackgroundJobStore } from "../../src/lib/background-jobs";
import { ImportQueue } from "../../src/lib/import-queue";

// Cancelling an import through cancelRemaining() moves the job to
// 'cancelling' and waits for an import-error event that the backend suppresses
// for user cancels. The local (capacity-1) pool therefore never frees.

const flush = () => new Promise((resolve) => setTimeout(resolve, 0));

function urlItem(n: number) {
  return {
    url: `https://t-my.sharepoint.com/_layouts/15/stream.aspx?id=/personal/u/Recordings/rec-${n}.mp4`,
    title: `Rec ${n}`,
  };
}

function makeInvoker(started: { command: string; args: Record<string, unknown> }[]) {
  return async (command: string, args: Record<string, unknown>) => {
    if (command.startsWith("cancel_")) {
      // Backend accepts the cancel request; per the backend, no import-error event follows.
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
    const queue = new ImportQueue(store, makeInvoker(started), (() => {
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
    const queue = new ImportQueue(store, makeInvoker(started), (() => {
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
