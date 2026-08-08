//! Hand-written SDL3 bindings (a "simple bindings layer").
//!
//! Covers the subset of SDL3 a small game needs: initialization, windows,
//! the 2D renderer, textures, events, keyboard/mouse input, and timers.
//! Every signature below was transcribed from the SDL3 headers shipped by
//! Homebrew (`/opt/homebrew/opt/sdl3/include/SDL3`); the struct layouts and
//! enum values are pinned by the comments next to each item.
//!
//! The native library itself comes from the `cc_import` target `:sdl3` —
//! Tong passes `-L native` and `-l SDL3.0` to every crate that (transitively)
//! depends on it and ships the dylib in the final binary's runtime closure.
//!
//! Anything SDL3 offers beyond this list is left to bindgen in a future
//! example; see `examples/04-voxel-city` for a game built on these bindings.

#![allow(non_camel_case_types)]

pub mod error;
pub mod events;
pub mod init;
pub mod input;
pub mod render;
pub mod timer;
pub mod window;

pub use error::last_error;

// Flat re-exports so consumers can use `sdl3_sys::SDL_*` directly, as the
// original single-file bindings did.
pub use events::*;
pub use init::*;
pub use input::*;
pub use render::*;
pub use timer::*;
pub use window::*;
