//! Window management.

use std::ffi::{c_char, c_int, c_void};

/// Opaque window handle.
pub type SDL_Window = c_void;

/// `SDL_WindowFlags` bitmask (`Uint64` in SDL3).
pub type SDL_WindowFlags = u64;

/// `SDL_WINDOW_RESIZABLE` (`0x0000000000000020`). SDL3 windows are shown by
/// default, so no `SDL_WINDOW_SHOWN` constant is needed.
pub const SDL_WINDOW_RESIZABLE: SDL_WindowFlags = 0x0000_0000_0000_0020;

/// `SDL_WINDOW_HIGH_PIXEL_DENSITY` (`0x0000000000002000`).
pub const SDL_WINDOW_HIGH_PIXEL_DENSITY: SDL_WindowFlags = 0x0000_0000_0000_2000;

extern "C" {
    /// Creates a window; returns null on failure.
    pub fn SDL_CreateWindow(
        title: *const c_char,
        width: c_int,
        height: c_int,
        flags: SDL_WindowFlags,
    ) -> *mut SDL_Window;
    /// Destroys a window.
    pub fn SDL_DestroyWindow(window: *mut SDL_Window);
    /// Returns the window's pixel size; true on success. Replaced SDL2's
    /// `SDL_GetWindowSize` in SDL3.
    pub fn SDL_GetWindowSizeInPixels(
        window: *mut SDL_Window,
        width: *mut c_int,
        height: *mut c_int,
    ) -> bool;
    /// Sets the window title; true on success.
    pub fn SDL_SetWindowTitle(window: *mut SDL_Window, title: *const c_char) -> bool;
}
