// Headers pinned to whisper-rs-sys 0.11.1. Do not update independently.
#include "whisper.h"
#include <exception>
#include <cstdio>

namespace {
thread_local char last_error[1024] = {};
void record(const char *op, const char *message) noexcept {
    std::snprintf(last_error, sizeof(last_error), "%s: %s", op, message);
    std::fprintf(stderr, "meetily_whisper_native_exception: %s\n", last_error);
}
template<class F, class R> R guarded(const char *op, R failure, F fn) noexcept {
    last_error[0] = 0;
    try { return fn(); }
    catch (const std::exception &e) { record(op, e.what()); }
    catch (...) { record(op, "unknown C++ exception"); }
    return failure;
}
}
extern "C" {
const char *meetily_whisper_last_error() noexcept { return last_error; }
whisper_context *meetily_whisper_load_file(const char *path, const whisper_context_params *params) noexcept {
    return guarded("load_model", (whisper_context *)nullptr, [&] { return whisper_init_from_file_with_params_no_state(path, *params); });
}
whisper_context *meetily_whisper_load_buffer(void *data, size_t size, const whisper_context_params *params) noexcept {
    return guarded("load_buffer", (whisper_context *)nullptr, [&] { return whisper_init_from_buffer_with_params_no_state(data, size, *params); });
}
whisper_state *meetily_whisper_init_state(whisper_context *ctx) noexcept {
    return guarded("create_state", (whisper_state *)nullptr, [&] { return whisper_init_state(ctx); });
}
int meetily_whisper_full(whisper_context *ctx, whisper_state *state, const whisper_full_params *params, const float *samples, int count) noexcept {
    return guarded("transcribe", -1001, [&] { return whisper_full_with_state(ctx, state, *params, samples, count); });
}
void meetily_whisper_free_state(whisper_state *state) noexcept {
    guarded("free_state", -1001, [&] { whisper_free_state(state); return 0; });
}
void meetily_whisper_free_context(whisper_context *ctx) noexcept {
    guarded("free_context", -1001, [&] { whisper_free(ctx); return 0; });
}
}
