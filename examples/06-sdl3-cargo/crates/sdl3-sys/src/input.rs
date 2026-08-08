//! Keyboard and mouse input state.
//!
//! SDL3's `SDL_GetKeyboardState` returns `const bool *` (SDL2 returned
//! `const Uint8 *`); the array is indexed by `SDL_Scancode`. Scancodes are
//! physical keys, so WASD works regardless of keyboard layout.

use std::ffi::c_int;

/// `SDL_Scancode` enum value (`int`); physical key positions.
pub type SDL_Scancode = c_int;

/// `SDL_SCANCODE_A` (4).
pub const SDL_SCANCODE_A: SDL_Scancode = 4;
/// `SDL_SCANCODE_D` (7).
pub const SDL_SCANCODE_D: SDL_Scancode = 7;
/// `SDL_SCANCODE_S` (22).
pub const SDL_SCANCODE_S: SDL_Scancode = 22;
/// `SDL_SCANCODE_W` (26).
pub const SDL_SCANCODE_W: SDL_Scancode = 26;
/// `SDL_SCANCODE_ESCAPE` (41).
pub const SDL_SCANCODE_ESCAPE: SDL_Scancode = 41;
/// `SDL_SCANCODE_SPACE` (44).
pub const SDL_SCANCODE_SPACE: SDL_Scancode = 44;
/// `SDL_SCANCODE_RIGHT` (79).
pub const SDL_SCANCODE_RIGHT: SDL_Scancode = 79;
/// `SDL_SCANCODE_LEFT` (80).
pub const SDL_SCANCODE_LEFT: SDL_Scancode = 80;
/// `SDL_SCANCODE_DOWN` (81).
pub const SDL_SCANCODE_DOWN: SDL_Scancode = 81;
/// `SDL_SCANCODE_UP` (82).
pub const SDL_SCANCODE_UP: SDL_Scancode = 82;

/// `SDL_Keycode` (`Uint32`); virtual keys. Only the values used by the
/// examples are exposed.
pub type SDL_Keycode = u32;

/// `SDLK_ESCAPE` (`0x1B`).
pub const SDLK_ESCAPE: SDL_Keycode = 0x1B;
/// `SDLK_PLUS` (`0x2B`).
pub const SDLK_PLUS: SDL_Keycode = 0x2B;
/// `SDLK_MINUS` (`0x2D`).
pub const SDLK_MINUS: SDL_Keycode = 0x2D;
/// `SDLK_R` (`0x72`).
pub const SDLK_R: SDL_Keycode = 0x72;

/// `SDL_MouseButtonFlags` (`Uint32`); a bitmask where button *n* is
/// `1 << (n - 1)`.
pub type SDL_MouseButtonFlags = u32;

/// `SDL_BUTTON_LEFT` (1).
pub const SDL_BUTTON_LEFT: u8 = 1;
/// `SDL_BUTTON_MIDDLE` (2).
pub const SDL_BUTTON_MIDDLE: u8 = 2;
/// `SDL_BUTTON_RIGHT` (3).
pub const SDL_BUTTON_RIGHT: u8 = 3;
/// `SDL_BUTTON_LMASK` — left-button bit in `SDL_MouseButtonFlags`.
pub const SDL_BUTTON_LMASK: SDL_MouseButtonFlags = 1 << (SDL_BUTTON_LEFT - 1);
/// `SDL_BUTTON_RMASK` — right-button bit in `SDL_MouseButtonFlags`.
pub const SDL_BUTTON_RMASK: SDL_MouseButtonFlags = 1 << (SDL_BUTTON_RIGHT - 1);

extern "C" {
    /// Returns a pointer to the current keyboard state, indexed by
    /// `SDL_Scancode`. `numkeys` receives the array length (may be null).
    pub fn SDL_GetKeyboardState(numkeys: *mut c_int) -> *const bool;
    /// Returns the current mouse button mask and, when the pointers are
    /// non-null, the cursor position relative to the window.
    pub fn SDL_GetMouseState(x: *mut f32, y: *mut f32) -> SDL_MouseButtonFlags;
}
