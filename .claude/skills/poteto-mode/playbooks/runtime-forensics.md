### Runtime forensics

**You own the diagnosis. Instrument the live process, don't theorize from source.** The deliverable is a cited diagnosis, not a fix.

1. Capture the live signal from the running app: `sample` or Instruments (Time Profiler, Allocations) on the `meetily` process for a spin or a leak, the WebView DevTools Performance or Memory panel (Cmd+Shift+I) for a UI glitch, `RUST_LOG=app_lib::audio=debug` for pipeline timing. A real artifact, not a guess.
2. Reduce the artifact to the smoking gun: the function on the hot path, the retainer chain from the leaked object to a GC root, the loop firing without input. Parse large artifacts in a subagent (**guard-the-context-window**), keep the reduced finding in the main thread.
3. Prove the mechanism before believing it. Add a targeted log line or counter (`perf_debug!` on hot paths), or evaluate in the WebView console, and rerun to confirm the hypothesis cheaply.
4. Map the finding back to source: file, symbol, the line that allocates or schedules.
5. Throughput checkpoint stays one line: `throughput checkpoint: n/a, read-only forensics`.

**Reply:** the signal captured, the reduced finding, how you proved the mechanism, the source location, artifact paths. No fix unless asked. Hand back to Bug fix or Perf once the cause is known.
