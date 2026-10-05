//! C++ exceptions must be caught before crossing into Rust. Only the high-level
//! load/state/full/drop path is guarded; raw-api and low-level methods are not.
use std::ffi::{c_char, c_int, c_void, CStr};
use whisper_rs_sys::{whisper_context, whisper_context_params, whisper_full_params, whisper_state};
extern "C" {
    pub(crate) fn meetily_whisper_load_file(
        path: *const c_char,
        params: *const whisper_context_params,
    ) -> *mut whisper_context;
    pub(crate) fn meetily_whisper_load_buffer(
        data: *mut c_void,
        size: usize,
        params: *const whisper_context_params,
    ) -> *mut whisper_context;
    pub(crate) fn meetily_whisper_init_state(ctx: *mut whisper_context) -> *mut whisper_state;
    pub(crate) fn meetily_whisper_full(
        ctx: *mut whisper_context,
        state: *mut whisper_state,
        params: *const whisper_full_params,
        samples: *const f32,
        count: c_int,
    ) -> c_int;
    pub(crate) fn meetily_whisper_free_state(state: *mut whisper_state);
    pub(crate) fn meetily_whisper_free_context(ctx: *mut whisper_context);
    fn meetily_whisper_last_error() -> *const c_char;
}
pub(crate) fn error_or(fallback: crate::WhisperError) -> crate::WhisperError {
    // Copy on the same thread immediately after the native call, before any
    // destructor can overwrite the thread-local diagnostic.
    let message = unsafe { CStr::from_ptr(meetily_whisper_last_error()) };
    if message.to_bytes().is_empty() {
        fallback
    } else {
        crate::WhisperError::NativeException(message.to_string_lossy().into_owned())
    }
}
