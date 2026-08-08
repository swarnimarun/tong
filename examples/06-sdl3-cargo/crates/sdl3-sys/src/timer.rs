//! Timers.

extern "C" {
    /// Milliseconds since SDL initialized. `Uint64` in SDL3 (SDL2's
    /// `SDL_GetTicks64` folded back into `SDL_GetTicks`).
    pub fn SDL_GetTicks() -> u64;
    /// Sleeps for the given number of milliseconds.
    pub fn SDL_Delay(ms: u32);
}
