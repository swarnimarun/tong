//! SDL error reporting.

use std::ffi::c_char;

extern "C" {
    /// Returns the last error message; the pointer is valid until the next
    /// SDL call. Returns a null pointer when there is no error.
    pub fn SDL_GetError() -> *const c_char;
}

/// The last SDL error as a Rust string.
pub fn last_error() -> String {
    unsafe {
        let ptr = SDL_GetError();
        if ptr.is_null() {
            return "unknown error".to_owned();
        }
        std::ffi::CStr::from_ptr(ptr).to_string_lossy().into_owned()
    }
}
