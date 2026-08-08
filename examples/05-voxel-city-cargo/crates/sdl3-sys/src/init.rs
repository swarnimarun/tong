//! Subsystem initialization and shutdown.

/// `SDL_InitFlags` bitmask (`typedef Uint32 SDL_InitFlags`).
pub type SDL_InitFlags = u32;

/// `SDL_INIT_VIDEO` (`0x00000020`).
pub const SDL_INIT_VIDEO: SDL_InitFlags = 0x0000_0020;

extern "C" {
    /// Initializes SDL subsystems; returns true on success. SDL3 uses C
    /// `bool`, which is one byte — matching Rust's `bool`.
    pub fn SDL_Init(flags: SDL_InitFlags) -> bool;
    /// Cleans up all initialized subsystems.
    pub fn SDL_Quit();
}
