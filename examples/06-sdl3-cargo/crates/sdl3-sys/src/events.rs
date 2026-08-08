//! The SDL event queue and event structs.
//!
//! `SDL_Event` is a C union. In Rust it is modeled as a union whose first
//! member (`type_`, a `u32`) is shared by every event; a 128-byte padding
//! member keeps the allocation the same size as SDL's. The struct layouts
//! below match the SDL3 headers exactly (offsets are documented per field).

/// `SDL_EVENT_QUIT` (`0x100`): user-requested quit.
pub const SDL_EVENT_QUIT: u32 = 0x100;
/// `SDL_EVENT_KEY_DOWN` (`0x300`).
pub const SDL_EVENT_KEY_DOWN: u32 = 0x300;
/// `SDL_EVENT_KEY_UP` (`0x301`).
pub const SDL_EVENT_KEY_UP: u32 = 0x301;
/// `SDL_EVENT_MOUSE_MOTION` (`0x400`).
pub const SDL_EVENT_MOUSE_MOTION: u32 = 0x400;
/// `SDL_EVENT_MOUSE_BUTTON_DOWN` (`0x401`).
pub const SDL_EVENT_MOUSE_BUTTON_DOWN: u32 = 0x401;
/// `SDL_EVENT_MOUSE_BUTTON_UP` (`0x402`).
pub const SDL_EVENT_MOUSE_BUTTON_UP: u32 = 0x402;
/// `SDL_EVENT_MOUSE_WHEEL` (`0x403`).
pub const SDL_EVENT_MOUSE_WHEEL: u32 = 0x403;
/// `SDL_EVENT_WINDOW_RESIZED` (`0x206`): window resized to `data1`x`data2`.
pub const SDL_EVENT_WINDOW_RESIZED: u32 = 0x206;
/// `SDL_EVENT_WINDOW_PIXEL_SIZE_CHANGED` (`0x207`).
pub const SDL_EVENT_WINDOW_PIXEL_SIZE_CHANGED: u32 = 0x207;
/// `SDL_EVENT_WINDOW_CLOSE_REQUESTED` (`0x210`): the window manager asked
/// the window to close.
pub const SDL_EVENT_WINDOW_CLOSE_REQUESTED: u32 = 0x210;

/// `SDL_CommonEvent` — fields shared by every event:
/// `{ Uint32 type; Uint32 reserved; Uint64 timestamp; }`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SDL_CommonEvent {
    pub type_: u32,
    pub reserved: u32,
    pub timestamp: u64,
}

/// `SDL_WindowEvent` — `{ type, reserved, timestamp, windowID, data1,
/// data2 }`; `data1`/`data2` carry event-specific values (e.g. the new
/// width and height for `SDL_EVENT_WINDOW_RESIZED`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SDL_WindowEvent {
    pub type_: u32,
    pub reserved: u32,
    pub timestamp: u64,
    pub window_id: u32,
    pub data1: i32,
    pub data2: i32,
}

/// `SDL_KeyboardEvent` — `{ type, reserved, timestamp, windowID, which,
/// scancode, key, mod, raw, down, repeat }`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SDL_KeyboardEvent {
    pub type_: u32,
    pub reserved: u32,
    pub timestamp: u64,
    pub window_id: u32,
    pub which: u32,
    pub scancode: i32,
    pub key: u32,
    pub mod_: u16,
    pub raw: u16,
    pub down: bool,
    pub repeat: bool,
}

/// `SDL_MouseButtonEvent` — `{ type, reserved, timestamp, windowID, which,
/// button, down, clicks, padding, x, y }`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SDL_MouseButtonEvent {
    pub type_: u32,
    pub reserved: u32,
    pub timestamp: u64,
    pub window_id: u32,
    pub which: u32,
    pub button: u8,
    pub down: bool,
    pub clicks: u8,
    pub padding: u8,
    pub x: f32,
    pub y: f32,
}

/// `SDL_MouseWheelEvent` — `{ type, reserved, timestamp, windowID, which,
/// x, y, direction, mouse_x, mouse_y, integer_x, integer_y }`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SDL_MouseWheelEvent {
    pub type_: u32,
    pub reserved: u32,
    pub timestamp: u64,
    pub window_id: u32,
    pub which: u32,
    pub x: f32,
    pub y: f32,
    pub direction: i32,
    pub mouse_x: f32,
    pub mouse_y: f32,
    pub integer_x: i32,
    pub integer_y: i32,
}

/// `SDL_Event` — the event union. SDL writes only the active member; the
/// first `u32` (`type_`) identifies which one.
#[repr(C)]
#[derive(Clone, Copy)]
pub union SDL_Event {
    /// Event type, shared with all events.
    pub type_: u32,
    /// Common event data.
    pub common: SDL_CommonEvent,
    /// Keyboard event (`SDL_EVENT_KEY_DOWN` / `SDL_EVENT_KEY_UP`).
    pub key: SDL_KeyboardEvent,
    /// Mouse button event (`SDL_EVENT_MOUSE_BUTTON_DOWN` / `_UP`).
    pub button: SDL_MouseButtonEvent,
    /// Mouse wheel event (`SDL_EVENT_MOUSE_WHEEL`).
    pub wheel: SDL_MouseWheelEvent,
    /// Window state event (`SDL_EVENT_WINDOW_*`).
    pub window: SDL_WindowEvent,
    /// Sized to SDL's 128-byte event allocation.
    pub padding: [u8; 128],
}

impl SDL_Event {
    /// Creates a zeroed event.
    pub fn new() -> Self {
        SDL_Event { padding: [0; 128] }
    }

    /// The event type (`type_` at offset 0 in every member).
    pub fn type_(&self) -> u32 {
        // SAFETY: `type_` is the first member of the union and of every
        // struct member, so it is always the initialized field.
        unsafe { self.type_ }
    }

    /// The keyboard event; valid only for key events.
    pub fn key(&self) -> &SDL_KeyboardEvent {
        // SAFETY: reading the `key` member is only valid for key events;
        // callers must check `type_` first.
        unsafe { &self.key }
    }

    /// The mouse button event; valid only for button events.
    pub fn button(&self) -> &SDL_MouseButtonEvent {
        // SAFETY: callers must check `type_` first.
        unsafe { &self.button }
    }

    /// The mouse wheel event; valid only for wheel events.
    pub fn wheel(&self) -> &SDL_MouseWheelEvent {
        // SAFETY: callers must check `type_` first.
        unsafe { &self.wheel }
    }

    /// The window event; valid only for window events.
    pub fn window(&self) -> &SDL_WindowEvent {
        // SAFETY: callers must check `type_` first.
        unsafe { &self.window }
    }
}

extern "C" {
    /// Pops the next event; returns false when the queue is empty.
    pub fn SDL_PollEvent(event: *mut SDL_Event) -> bool;
}
