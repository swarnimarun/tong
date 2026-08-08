//! Rectangles, the 2D renderer, and textures.
//!
//! SDL3's renderer APIs return `bool` (SDL2 returned `int` or `void`) and
//! the drawing functions take float rectangles (`SDL_FRect`); both changes
//! are reflected here.

use std::ffi::{c_char, c_int, c_void};

use crate::window::SDL_Window;

/// `SDL_Rect` — integer rectangle `{ int x, y, w, h; }`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SDL_Rect {
    pub x: c_int,
    pub y: c_int,
    pub w: c_int,
    pub h: c_int,
}

/// `SDL_FRect` — float rectangle `{ float x, y, w, h; }`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SDL_FRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Opaque renderer handle.
pub type SDL_Renderer = c_void;
/// Opaque texture handle.
pub type SDL_Texture = c_void;

/// `SDL_PixelFormat` enum value (`Uint32`).
pub type SDL_PixelFormat = u32;

/// `SDL_PIXELFORMAT_RGBA8888` (`0x16462004`): 32-bit RGBA, byte order
/// R,G,B,A. The format used by the voxel-city backbuffer.
pub const SDL_PIXELFORMAT_RGBA8888: SDL_PixelFormat = 0x1646_2004;

/// `SDL_TextureAccess` enum (`int`).
pub type SDL_TextureAccess = c_int;

/// `SDL_TEXTUREACCESS_STATIC`.
pub const SDL_TEXTUREACCESS_STATIC: SDL_TextureAccess = 0;
/// `SDL_TEXTUREACCESS_STREAMING` — updated frequently via `SDL_UpdateTexture`.
pub const SDL_TEXTUREACCESS_STREAMING: SDL_TextureAccess = 1;
/// `SDL_TEXTUREACCESS_TARGET` — renderable as a render target.
pub const SDL_TEXTUREACCESS_TARGET: SDL_TextureAccess = 2;

extern "C" {
    /// Creates a renderer for the window. `name` may be null to let SDL
    /// choose; SDL3 dropped the SDL2 `flags` parameter.
    pub fn SDL_CreateRenderer(window: *mut SDL_Window, name: *const c_char) -> *mut SDL_Renderer;
    /// Destroys a renderer.
    pub fn SDL_DestroyRenderer(renderer: *mut SDL_Renderer);
    /// Sets the color used by `SDL_RenderClear`, `SDL_RenderRect`, and
    /// `SDL_RenderFillRect`.
    pub fn SDL_SetRenderDrawColor(
        renderer: *mut SDL_Renderer,
        r: u8,
        g: u8,
        b: u8,
        a: u8,
    ) -> bool;
    /// Clears the render target to the current draw color.
    pub fn SDL_RenderClear(renderer: *mut SDL_Renderer) -> bool;
    /// Presents the current frame.
    pub fn SDL_RenderPresent(renderer: *mut SDL_Renderer) -> bool;
    /// Draws a line.
    pub fn SDL_RenderLine(
        renderer: *mut SDL_Renderer,
        x1: f32,
        y1: f32,
        x2: f32,
        y2: f32,
    ) -> bool;
    /// Draws a rectangle outline.
    pub fn SDL_RenderRect(renderer: *mut SDL_Renderer, rect: *const SDL_FRect) -> bool;
    /// Fills a rectangle.
    pub fn SDL_RenderFillRect(renderer: *mut SDL_Renderer, rect: *const SDL_FRect) -> bool;
    /// Enables (`1`) or disables (`0`) vertical sync.
    pub fn SDL_SetRenderVSync(renderer: *mut SDL_Renderer, vsync: c_int) -> bool;

    /// Creates a texture; returns null on failure.
    pub fn SDL_CreateTexture(
        renderer: *mut SDL_Renderer,
        format: SDL_PixelFormat,
        access: SDL_TextureAccess,
        width: c_int,
        height: c_int,
    ) -> *mut SDL_Texture;
    /// Destroys a texture.
    pub fn SDL_DestroyTexture(texture: *mut SDL_Texture);
    /// Uploads pixel data to a texture. `rect` may be null for the whole
    /// texture; `pitch` is the bytes per row.
    pub fn SDL_UpdateTexture(
        texture: *mut SDL_Texture,
        rect: *const SDL_Rect,
        pixels: *const c_void,
        pitch: c_int,
    ) -> bool;
    /// Copies a texture region to the render target (SDL3 renamed SDL2's
    /// `SDL_RenderCopy`). Null rects mean the whole texture / full target.
    pub fn SDL_RenderTexture(
        renderer: *mut SDL_Renderer,
        texture: *mut SDL_Texture,
        srcrect: *const SDL_FRect,
        dstrect: *const SDL_FRect,
    ) -> bool;
}
