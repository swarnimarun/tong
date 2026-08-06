//! Minimal hand-written SDL3 bindings (a "simple bindings layer").
//!
//! Only the functions the demo needs; the rest of SDL3 is left to bindgen
//! in a future example. The native library itself comes from the
//! `cc_import` target `:sdl3` — Tong passes `-L native` and `-l SDL3.0`
//! to this crate's compile action and propagates the library to the final
//! binary's runtime closure.

#![allow(non_camel_case_types)]

use std::ffi::c_void;
use std::os::raw::{c_char, c_int};

/// Opaque window handle.
pub type SDL_Window = c_void;

/// `SDL_InitFlags` bitmask.
pub type SDL_InitFlags = u32;

/// `SDL_INIT_VIDEO`.
pub const SDL_INIT_VIDEO: SDL_InitFlags = 0x00000020;

/// `SDL_WindowFlags` (u64 in SDL3).
pub type SDL_WindowFlags = u64;

extern "C" {
    /// Initializes SDL subsystems; returns true on success.
    pub fn SDL_Init(flags: SDL_InitFlags) -> bool;
    /// Cleans up all initialized subsystems.
    pub fn SDL_Quit();
    /// Creates a window; returns null on failure.
    pub fn SDL_CreateWindow(
        title: *const c_char,
        width: c_int,
        height: c_int,
        flags: SDL_WindowFlags,
    ) -> *mut SDL_Window;
    /// Destroys a window.
    pub fn SDL_DestroyWindow(window: *mut SDL_Window);
    /// Sleeps for the given number of milliseconds.
    pub fn SDL_Delay(ms: u32);
    /// Returns the last error message.
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
