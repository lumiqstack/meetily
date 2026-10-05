// Fake native backend exercises the production C++/Rust boundary without a GPU.
#include "whisper.h"
#include <stdexcept>
#include <cstring>
static int mode = 0;
static int selected_gpu = -1;
extern "C" {
void test_mode(int value) { mode = value; }
int test_selected_gpu() { return selected_gpu; }
static void maybe_throw() { if (mode == 1) throw std::runtime_error("injected Vulkan device loss"); if (mode == 2) throw 42; }
whisper_context *whisper_init_from_file_with_params_no_state(const char *, whisper_context_params p) { selected_gpu = p.use_gpu; maybe_throw(); return (whisper_context *)1; }
whisper_context *whisper_init_from_buffer_with_params_no_state(void *, size_t, whisper_context_params p) { selected_gpu = p.use_gpu; maybe_throw(); return (whisper_context *)1; }
whisper_state *whisper_init_state(whisper_context *) { maybe_throw(); return (whisper_state *)2; }
int whisper_full_with_state(whisper_context *, whisper_state *, whisper_full_params, const float *, int) { maybe_throw(); return 0; }
void whisper_free_state(whisper_state *) { maybe_throw(); }
void whisper_free(whisper_context *) { maybe_throw(); }
whisper_context *meetily_whisper_load_file(const char *, const whisper_context_params *) noexcept;
whisper_context *meetily_whisper_load_buffer(void *, size_t, const whisper_context_params *) noexcept;
int meetily_whisper_full(whisper_context *, whisper_state *, const whisper_full_params *, const float *, int) noexcept;
int test_load_file() { whisper_context_params p = {}; p.use_gpu = true; return meetily_whisper_load_file("mock", &p) != nullptr; }
int test_load_buffer() { whisper_context_params p = {}; p.use_gpu = true; return meetily_whisper_load_buffer(nullptr, 0, &p) != nullptr; }
int test_full() { whisper_full_params p = {}; return meetily_whisper_full(nullptr, nullptr, &p, nullptr, 0); }
}
