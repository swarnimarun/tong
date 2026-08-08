//! SDL3 demo: opens a window, draws with the 2D renderer, polls events,
//! then exits cleanly.
//!
//! Build with `tong build` inside examples/03-sdl3; run with
//! `tong run :sdl3_demo`. Use `SDL_VIDEODRIVER=dummy` for a headless
//! run.

use sdl3_sys::{
    SDL_CreateRenderer, SDL_CreateWindow, SDL_Delay, SDL_DestroyRenderer, SDL_DestroyWindow,
    SDL_GetTicks, SDL_Init, SDL_INIT_VIDEO, SDL_PollEvent, SDL_Quit, SDL_RenderClear,
    SDL_RenderFillRect, SDL_RenderPresent, SDL_SetRenderDrawColor, SDL_Event, SDL_FRect,
    SDL_EVENT_QUIT, SDL_WINDOW_RESIZABLE, last_error,
};
use std::ffi::CString;

const WIDTH: i32 = 640;
const HEIGHT: i32 = 480;

fn main() {
    unsafe {
        if !SDL_Init(SDL_INIT_VIDEO) {
            panic!("SDL_Init failed: {}", last_error());
        }

        let title = CString::new("tong + SDL3").expect("title");
        let window = SDL_CreateWindow(title.as_ptr(), WIDTH, HEIGHT, SDL_WINDOW_RESIZABLE);
        if window.is_null() {
            panic!("SDL_CreateWindow failed: {}", last_error());
        }
        let renderer = SDL_CreateRenderer(window, std::ptr::null());
        if renderer.is_null() {
            panic!("SDL_CreateRenderer failed: {}", last_error());
        }

        println!("SDL3 window created at {window:p}; drawing for 1.5s...");

        let mut quit = false;
        let deadline = SDL_GetTicks() + 1500;
        while !quit && SDL_GetTicks() < deadline {
            let mut event = SDL_Event::new();
            while SDL_PollEvent(&mut event) {
                if event.type_() == SDL_EVENT_QUIT {
                    quit = true;
                }
            }
            SDL_SetRenderDrawColor(renderer, 24, 28, 40, 255);
            SDL_RenderClear(renderer);
            SDL_SetRenderDrawColor(renderer, 94, 196, 120, 255);
            let grass = SDL_FRect {
                x: 32.0,
                y: 32.0,
                w: 320.0,
                h: 180.0,
            };
            SDL_RenderFillRect(renderer, &grass);
            SDL_SetRenderDrawColor(renderer, 240, 200, 80, 255);
            let sun = SDL_FRect {
                x: 480.0,
                y: 48.0,
                w: 96.0,
                h: 96.0,
            };
            SDL_RenderFillRect(renderer, &sun);
            SDL_RenderPresent(renderer);
            SDL_Delay(16);
        }

        SDL_DestroyRenderer(renderer);
        SDL_DestroyWindow(window);
        SDL_Quit();
        println!("sdl3 demo finished cleanly");
    }
}
