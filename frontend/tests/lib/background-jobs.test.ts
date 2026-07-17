import { describe, expect, test } from "bun:test";

import {
  BackgroundJobStore,
  cleanupDelayMs,
  toastDurationMs,
} from "../../src/lib/background-jobs";

describe("BackgroundJobStore registration", () => {
  test("registerImport exposes a running import job with its title", () => {
    const store = new BackgroundJobStore();

    store.registerImport("import-abc", "Standup Recording");

    const jobs = store.getJobs();
    expect(jobs).toHaveLength(1);
    expect(jobs[0]).toEqual({
      id: "import-abc",
      kind: "import",
      title: "Standup Recording",
      status: "running",
      progressPercentage: 0,
      message: "",
      error: null,
    });
    expect(store.has("import-abc")).toBe(true);
  });

  test("registerRetranscription exposes a running retranscription job, defaulting the title", () => {
    const store = new BackgroundJobStore();

    store.registerRetranscription("meeting-1");
    store.registerRetranscription("meeting-2", "Weekly Sync");

    const jobs = store.getJobs();
    expect(jobs).toHaveLength(2);
    expect(jobs[0]).toMatchObject({
      id: "meeting-1",
      kind: "retranscription",
      title: "Retranscription",
      status: "running",
    });
    expect(jobs[1]).toMatchObject({ id: "meeting-2", title: "Weekly Sync" });
  });
});

describe("BackgroundJobStore progress", () => {
  test("applyProgress updates a tracked job and reports it was tracked", () => {
    const store = new BackgroundJobStore();
    store.registerImport("import-abc", "Standup Recording");

    const tracked = store.applyProgress("import-abc", 42, "Transcribing chunk 3/7...");

    expect(tracked).toBe(true);
    expect(store.getJobs()[0]).toMatchObject({
      status: "running",
      progressPercentage: 42,
      message: "Transcribing chunk 3/7...",
    });
  });

  test("applyProgress ignores unknown job ids without creating a job", () => {
    const store = new BackgroundJobStore();

    const tracked = store.applyProgress("import-foreground", 10, "Decoding...");

    expect(tracked).toBe(false);
    expect(store.getJobs()).toHaveLength(0);
  });
});

describe("BackgroundJobStore completion", () => {
  test("applyComplete marks a tracked job completed at 100%", () => {
    const store = new BackgroundJobStore();
    store.registerImport("import-abc", "Standup Recording");
    store.applyProgress("import-abc", 42, "Transcribing...");

    const tracked = store.applyComplete("import-abc");

    expect(tracked).toBe(true);
    expect(store.getJobs()[0]).toMatchObject({
      status: "completed",
      progressPercentage: 100,
    });
  });

  test("applyComplete ignores unknown job ids", () => {
    const store = new BackgroundJobStore();

    expect(store.applyComplete("import-foreground")).toBe(false);
    expect(store.getJobs()).toHaveLength(0);
  });
});

describe("BackgroundJobStore errors and cancellation", () => {
  test("applyError marks a running job as failed with the error message", () => {
    const store = new BackgroundJobStore();
    store.registerImport("import-abc", "Standup Recording");

    const tracked = store.applyError("import-abc", "Remote endpoint returned 500");

    expect(tracked).toBe(true);
    expect(store.getJobs()[0]).toMatchObject({
      status: "error",
      error: "Remote endpoint returned 500",
    });
  });

  test("applyError on a cancelling job marks it cancelled, not failed", async () => {
    const store = new BackgroundJobStore();
    store.registerImport("import-abc", "Standup Recording");
    await store.cancelJob("import-abc", async () => {});

    // Backend reports a cancelled job via import-error("Import cancelled")
    store.applyError("import-abc", "Import cancelled");

    expect(store.getJobs()[0]).toMatchObject({
      status: "cancelled",
      error: null,
    });
  });

  test("applyError ignores unknown job ids", () => {
    const store = new BackgroundJobStore();

    expect(store.applyError("import-foreground", "boom")).toBe(false);
    expect(store.getJobs()).toHaveLength(0);
  });

  test("cancelJob invokes cancel_import_command for imports and marks the job cancelling", async () => {
    const store = new BackgroundJobStore();
    store.registerImport("import-abc", "Standup Recording");
    const calls: Array<{ command: string; args: Record<string, string> }> = [];

    const cancelled = await store.cancelJob("import-abc", async (command, args) => {
      calls.push({ command, args });
    });

    expect(cancelled).toBe(true);
    expect(calls).toEqual([
      { command: "cancel_import_command", args: { importId: "import-abc" } },
    ]);
    expect(store.getJobs()[0]).toMatchObject({ status: "cancelling" });
  });

  test("cancelJob invokes cancel_retranscription_command for retranscriptions", async () => {
    const store = new BackgroundJobStore();
    store.registerRetranscription("meeting-1");
    const calls: Array<{ command: string; args: Record<string, string> }> = [];

    const cancelled = await store.cancelJob("meeting-1", async (command, args) => {
      calls.push({ command, args });
    });

    expect(cancelled).toBe(true);
    expect(calls).toEqual([
      { command: "cancel_retranscription_command", args: { meetingId: "meeting-1" } },
    ]);
    expect(store.getJobs()[0]).toMatchObject({ status: "cancelling" });
  });

  test("cancelJob does not invoke anything for unknown job ids", async () => {
    const store = new BackgroundJobStore();
    let invoked = false;

    const cancelled = await store.cancelJob("nope", async () => {
      invoked = true;
    });

    expect(cancelled).toBe(false);
    expect(invoked).toBe(false);
  });

  test("cancelJob restores the running status when the cancel command fails", async () => {
    const store = new BackgroundJobStore();
    store.registerImport("import-abc", "Standup Recording");

    const cancelled = await store.cancelJob("import-abc", async () => {
      throw new Error("ipc failure");
    });

    expect(cancelled).toBe(false);
    expect(store.getJobs()[0]).toMatchObject({ status: "running" });
  });
});

describe("BackgroundJobStore removal", () => {
  test("remove drops the job and late events on it are ignored", () => {
    const store = new BackgroundJobStore();
    store.registerImport("import-abc", "Standup Recording");
    store.applyComplete("import-abc");

    store.remove("import-abc");

    expect(store.getJobs()).toHaveLength(0);
    expect(store.has("import-abc")).toBe(false);
    expect(store.applyProgress("import-abc", 50, "late event")).toBe(false);
    expect(store.getJobs()).toHaveLength(0);
  });
});

describe("BackgroundJobStore subscriptions", () => {
  test("notifies subscribers on mutations but not on ignored events", () => {
    const store = new BackgroundJobStore();
    let notifications = 0;
    const unsubscribe = store.subscribe(() => {
      notifications += 1;
    });

    store.registerImport("import-abc", "Standup Recording");
    store.applyProgress("import-abc", 10, "Decoding...");
    store.applyProgress("unknown-id", 10, "ignored");
    store.applyComplete("unknown-id");
    store.applyError("unknown-id", "ignored");
    store.applyComplete("import-abc");
    store.remove("import-abc");

    expect(notifications).toBe(4); // register, progress, complete, remove

    unsubscribe();
    store.registerImport("import-def", "Another");
    expect(notifications).toBe(4);
  });

  test("getJobs returns a stable snapshot reference until the next mutation", () => {
    const store = new BackgroundJobStore();
    store.registerImport("import-abc", "Standup Recording");

    const first = store.getJobs();
    const second = store.getJobs();
    expect(second).toBe(first);

    store.applyProgress("import-abc", 10, "Decoding...");
    const third = store.getJobs();
    expect(third).not.toBe(first);
    expect(third[0].progressPercentage).toBe(10);
  });
});

describe("toast presentation timings", () => {
  test("active jobs keep their toast open until manually handled", () => {
    expect(toastDurationMs("running")).toBe(Infinity);
    expect(toastDurationMs("cancelling")).toBe(Infinity);
  });

  test("terminal jobs auto-dismiss, errors staying longest", () => {
    expect(toastDurationMs("completed")).toBe(3000);
    expect(toastDurationMs("cancelled")).toBe(5000);
    expect(toastDurationMs("error")).toBe(10000);
  });

  test("cleanup delay outlives the toast by one second so it is not cut short", () => {
    expect(cleanupDelayMs("completed")).toBe(4000);
    expect(cleanupDelayMs("cancelled")).toBe(6000);
    expect(cleanupDelayMs("error")).toBe(11000);
  });
});
