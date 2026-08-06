//! SDL3 demo: opens a window briefly, then exits cleanly.
//!
//! Build with `tong build` inside examples/03-sdl3; run with
//! `tong run :sdl3_demo`. Use `SDL_VIDEODRIVER=dummy` for a headless
//! run.

use sdl3_sys::{SDL_INIT_VIDEO, SDL_Init, SDL_CreateWindow, SDL_Delay, SDL_DestroyWindow, SDL_Quit, last_error};
use std::ffi::CString;

fn main() {
    unsafe {
        if !SDL_Init(SDL_INIT_VIDEO) {
            panic!("SDL_Init failed: {}", last_error());
        }

        let title = CString::new("tong + SDL3").expect("title");
        let window = SDL_CreateWindow(title.as_ptr(), 640, 480, 0);
        if window.is_null() {
            panic!("SDL_CreateWindow failed: {}", last_error());
        }
        println!("SDL3 window created at {window:p}; waiting 1.5s...");
        SDL_Delay(1500);

        SDL_DestroyWindow(window);
        SDL_Quit();
        println!("sdl3 demo finished cleanly");
    }
}
