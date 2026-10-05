use std::ffi::{c_char, c_void, CStr};
extern "C" {
    fn test_mode(mode: i32);
    fn test_selected_gpu() -> i32;
    fn test_load_file() -> i32;
    fn test_load_buffer() -> i32;
    fn test_full() -> i32;
    fn meetily_whisper_init_state(ctx: *mut c_void) -> *mut c_void;
    fn meetily_whisper_free_state(state: *mut c_void);
    fn meetily_whisper_free_context(ctx: *mut c_void);
    fn meetily_whisper_last_error() -> *const c_char;
}
fn error() -> String {
    unsafe {
        CStr::from_ptr(meetily_whisper_last_error())
            .to_string_lossy()
            .into_owned()
    }
}
#[test]
fn native_exceptions_never_cross_into_rust_and_gpu_is_preserved() {
    unsafe {
        for mode in [1, 2] {
            test_mode(mode);
            assert_eq!(test_load_file(), 0);
            assert!(error().contains("load_model"));
            assert_eq!(test_selected_gpu(), 1);
            assert_eq!(test_load_buffer(), 0);
            assert!(error().contains("load_buffer"));
            assert!(meetily_whisper_init_state(std::ptr::null_mut()).is_null());
            assert!(error().contains("create_state"));
            assert_eq!(test_full(), -1001);
            assert!(error().contains(if mode == 1 {
                "injected Vulkan device loss"
            } else {
                "unknown C++ exception"
            }));
            meetily_whisper_free_state(std::ptr::null_mut());
            assert!(error().contains("free_state"));
            meetily_whisper_free_context(std::ptr::null_mut());
            assert!(error().contains("free_context"));
        }
        test_mode(0);
        assert_eq!(test_load_file(), 1);
        assert_eq!(test_selected_gpu(), 1);
        assert_eq!(error(), "");
        assert_eq!(test_load_buffer(), 1);
        assert!(!meetily_whisper_init_state(std::ptr::null_mut()).is_null());
        assert_eq!(test_full(), 0);
        assert_eq!(error(), "");
        meetily_whisper_free_state(std::ptr::null_mut());
        meetily_whisper_free_context(std::ptr::null_mut());
        assert_eq!(error(), "");
    }
}
#[path = "../../frontend/src-tauri/src/whisper_engine/state_cache.rs"]
mod state_cache;
