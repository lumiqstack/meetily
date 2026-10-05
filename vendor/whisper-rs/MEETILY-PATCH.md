# Meetily native exception boundary

This directory is whisper-rs 0.13.2 from crates.io (crate SHA256
40b6fc553156b521663bfa8e713e7ad58c7ca262d46de9998cd7f2e4de5ba0d9),
with its license and upstream source retained. The root Cargo patch selects it.
It pins whisper-rs-sys to exactly 0.11.1. The two headers under native/ come
unchanged from that sys crate; keep their ABI synchronized with that exact version.
Native Whisper/GGML source and GPU kernels are otherwise unchanged.

Local changes:
- Build native/guard.cpp with the same target C++ toolchain and MSVC /EHsc.
- Route high-level model loading, state creation, full inference and destruction
  through C++ try/catch wrappers. Preserve GPU parameters, return ordinary error
  sentinels, and copy the exception text into an owned NativeException error on
  the calling thread. Non-standard C++ exceptions receive a fixed diagnostic.
- Native destruction exceptions are logged and suppressed at the boundary;
  partially released allocations may remain until process exit. Do not repeatedly
  retry an engine after native failure. Meetily quarantines failed inference state.
- WhisperError is Clone, no longer Copy, because NativeException owns its message.

This does not catch access violations, explicit aborts/assertions, exceptions
escaping other native worker threads, or all low-level/raw API methods. It does
not promise recovery of a broken GPU device. No CPU fallback is introduced.

`python3 tests/whisper-native/run.py` (from repository root) builds the actual
production guard against a throwing test backend, then calls it from Rust. The
suite covers standard/unknown exceptions on all six guarded operations, success,
GPU flag preservation, state reuse, and failure quarantine/reset.

The app reuses one serialized inference state per loaded context. Previously a
new state/backend and compute buffers were created and freed for each chunk.
This reduces allocation churn but is not, by itself, proof of a fix on Iris Xe.
Real Windows transcription completion and timing remain required.
