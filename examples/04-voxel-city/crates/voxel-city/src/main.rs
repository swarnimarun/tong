//! voxel-city: a tiny isometric voxel city builder.
//!
//! Built on the shared `sdl3-sys` bindings from `examples/03-sdl3`, which
//! `Tong.toml` imports as an external crate root. A procedural seed city
//! (ground, pond, buildings, trees) is rendered with a painter's-algorithm
//! 2:1 isometric projection into a streaming RGBA8888 texture; the mouse
//! places and removes blocks.
//!
//! Controls:
//! - left click: place a block on the hovered column
//! - right click: remove the top block of the hovered column
//! - keys 1-7: choose the brush (grass, dirt, stone, wood, leaf, water, brick)
//! - ESC or window close: quit
//!
//! Build with `tong build` inside examples/04-voxel-city; run with
//! `tong run :voxel_city`. Headless: `SDL_VIDEODRIVER=dummy tong run
//! :voxel_city -- --frames 120` exits after 120 frames.

use sdl3_sys::*;
use std::ffi::{c_int, c_void, CString};

const WORLD_W: i32 = 40;
const WORLD_D: i32 = 40;
const WORLD_H: usize = 16;

// Block ids.
const AIR: u8 = 0;
const GRASS: u8 = 1;
const DIRT: u8 = 2;
const STONE: u8 = 3;
const WOOD: u8 = 4;
const LEAF: u8 = 5;
const WATER: u8 = 6;
const BRICK: u8 = 7;

const TILE_W: i32 = 24;
const TILE_H: i32 = 12;
const WIN_W: usize = 960;
const WIN_H: usize = 640;
const ORIGIN_X: f32 = 480.0;
const ORIGIN_Y: f32 = 150.0;

#[derive(Clone, Copy)]
struct Color(u8, u8, u8);

/// A block's colors: (top, south, east, west, north) — south faces the
/// camera, north faces away.
fn palette(id: u8) -> (Color, Color, Color, Color, Color) {
    match id {
        GRASS => (
            Color(94, 168, 78),
            Color(74, 138, 62),
            Color(66, 124, 55),
            Color(52, 98, 44),
            Color(42, 80, 36),
        ),
        DIRT => (
            Color(158, 112, 68),
            Color(128, 90, 56),
            Color(114, 80, 50),
            Color(92, 64, 40),
            Color(74, 52, 32),
        ),
        STONE => (
            Color(150, 150, 150),
            Color(122, 122, 122),
            Color(110, 110, 110),
            Color(90, 90, 90),
            Color(74, 74, 74),
        ),
        WOOD => (
            Color(154, 114, 72),
            Color(126, 92, 58),
            Color(112, 82, 52),
            Color(90, 66, 42),
            Color(72, 54, 34),
        ),
        LEAF => (
            Color(66, 148, 62),
            Color(54, 120, 50),
            Color(48, 108, 45),
            Color(38, 88, 36),
            Color(32, 74, 30),
        ),
        WATER => (
            Color(56, 128, 208),
            Color(56, 128, 208),
            Color(56, 128, 208),
            Color(56, 128, 208),
            Color(56, 128, 208),
        ),
        BRICK => (
            Color(184, 100, 74),
            Color(152, 82, 60),
            Color(136, 72, 53),
            Color(110, 58, 43),
            Color(90, 48, 35),
        ),
        _ => (
            Color(255, 0, 255),
            Color(255, 0, 255),
            Color(255, 0, 255),
            Color(255, 0, 255),
            Color(255, 0, 255),
        ),
    }
}

struct World {
    blocks: Vec<u8>,
    /// Top count per column: number of blocks (0 = empty column).
    top: Vec<u8>,
}

impl World {
    fn new() -> Self {
        let mut world = World {
            blocks: vec![AIR; WORLD_W as usize * WORLD_D as usize * WORLD_H],
            top: vec![0; WORLD_W as usize * WORLD_D as usize],
        };
        world.generate();
        world
    }

    fn index(x: i32, z: i32, y: i32) -> usize {
        (y as usize * WORLD_D as usize + z as usize) * WORLD_W as usize + x as usize
    }

    fn col(x: i32, z: i32) -> usize {
        z as usize * WORLD_W as usize + x as usize
    }

    fn get(&self, x: i32, z: i32, y: i32) -> u8 {
        if x < 0 || z < 0 || y < 0 || x >= WORLD_W || z >= WORLD_D || y >= WORLD_H as i32 {
            return AIR;
        }
        self.blocks[Self::index(x, z, y)]
    }

    fn set(&mut self, x: i32, z: i32, y: i32, id: u8) {
        if x < 0 || z < 0 || y < 0 || x >= WORLD_W || z >= WORLD_D || y >= WORLD_H as i32 {
            return;
        }
        self.blocks[Self::index(x, z, y)] = id;
        self.refresh_top(x, z);
    }

    fn refresh_top(&mut self, x: i32, z: i32) {
        let mut t = 0u8;
        for y in (0..WORLD_H as i32).rev() {
            if self.blocks[Self::index(x, z, y)] != AIR {
                t = (y + 1) as u8;
                break;
            }
        }
        self.top[Self::col(x, z)] = t;
    }

    /// Top count of a column; out-of-bounds columns count as empty.
    fn top_at(&self, x: i32, z: i32) -> i32 {
        if x < 0 || z < 0 || x >= WORLD_W || z >= WORLD_D {
            return 0;
        }
        self.top[Self::col(x, z)] as i32
    }

    fn place(&mut self, x: i32, z: i32, brush: u8) {
        let t = self.top_at(x, z);
        if t < WORLD_H as i32 {
            self.set(x, z, t, brush);
        }
    }

    fn remove(&mut self, x: i32, z: i32) {
        let t = self.top_at(x, z);
        if t > 0 {
            self.set(x, z, t - 1, AIR);
        }
    }

    /// Deterministic seed city: ground, a pond, a few buildings, trees.
    fn generate(&mut self) {
        for x in 0..WORLD_W {
            for z in 0..WORLD_D {
                let g = Self::ground_height(x, z);
                for y in 0..g {
                    self.set(x, z, y, DIRT);
                }
                self.set(x, z, g, GRASS);
            }
        }
        // Pond: flatten a shallow basin around it (the steep 45° view
        // otherwise hides the water behind taller ground in front), then
        // flood the disc itself.
        let (px, pz) = (11, 13);
        for x in 0..WORLD_W {
            for z in 0..WORLD_D {
                let dx = x - px;
                let dz = z - pz;
                let d2 = dx * dx + dz * dz;
                if d2 <= 25 {
                    let g = self.top_at(x, z) - 1;
                    for y in 0..=g {
                        self.set(x, z, y, AIR);
                    }
                    self.set(x, z, 0, GRASS);
                }
                if d2 <= 16 {
                    self.set(x, z, 0, WATER);
                }
            }
        }
        for (bx, bz, bw, bd, bh) in [
            (5, 5, 4, 4, 4),
            (30, 12, 5, 4, 5),
            (18, 30, 4, 5, 3),
            (33, 33, 3, 3, 5),
        ] {
            self.building(bx, bz, bw, bd, bh);
        }
        for (tx, tz) in [
            (3, 8),
            (8, 3),
            (15, 22),
            (22, 12),
            (26, 25),
            (31, 20),
            (8, 35),
            (36, 6),
            (24, 36),
            (37, 30),
        ] {
            self.tree(tx, tz);
        }
    }

    fn ground_height(x: i32, z: i32) -> i32 {
        2 + (hash2(x / 3, z / 3, 1) % 3) as i32 + (hash2(x / 7, z / 7, 2) % 2) as i32
    }

    fn building(&mut self, bx: i32, bz: i32, bw: i32, bd: i32, bh: i32) {
        for x in bx..bx + bw {
            for z in bz..bz + bd {
                let g = self.top_at(x, z);
                for y in 0..bh {
                    let id = if y + 1 == bh { STONE } else { BRICK };
                    self.set(x, z, g + y, id);
                }
            }
        }
    }

    fn tree(&mut self, tx: i32, tz: i32) {
        let g = self.top_at(tx, tz);
        self.set(tx, tz, g, WOOD);
        self.set(tx, tz, g + 1, WOOD);
        self.set(tx, tz, g + 2, WOOD);
        for x in tx - 1..=tx + 1 {
            for z in tz - 1..=tz + 1 {
                self.set(x, z, g + 3, LEAF);
                self.set(x, z, g + 2, if x == tx && z == tz { WOOD } else { LEAF });
            }
        }
    }
}

/// Integer hash for the deterministic terrain.
fn hash2(x: i32, z: i32, seed: i32) -> u32 {
    let mut h = (x.wrapping_mul(0x9E37_79B1_u32 as i32)
        ^ z.wrapping_mul(0x85EB_CA77_u32 as i32)
        ^ seed.wrapping_mul(0xC2B2_AE3D_u32 as i32)) as u32;
    h ^= h >> 16;
    h = h.wrapping_mul(0x7FEB_352D);
    h ^= h >> 15;
    h = h.wrapping_mul(0x846C_A68B);
    h ^= h >> 16;
    h
}

/// RGBA8888 packing (byte order R,G,B,A in memory, little-endian value).
fn pack(c: Color) -> u32 {
    (c.0 as u32) | ((c.1 as u32) << 8) | ((c.2 as u32) << 16) | (0xFF << 24)
}

fn blend(dst: u32, src: u32, alpha: u32) -> u32 {
    let inv = 255 - alpha;
    let r = (((src & 0xFF) * alpha) + ((dst & 0xFF) * inv)) / 255;
    let g = ((((src >> 8) & 0xFF) * alpha) + (((dst >> 8) & 0xFF) * inv)) / 255;
    let b = ((((src >> 16) & 0xFF) * alpha) + (((dst >> 16) & 0xFF) * inv)) / 255;
    0xFF00_0000 | (b << 16) | (g << 8) | r
}

/// Fills a convex quad with a scanline sweep (top and bottom edges are
/// always horizontal in this projection; the general form handles any
/// convex quad).
fn fill_quad(buf: &mut [u32], pts: &[(f32, f32); 4], color: u32, alpha: u32) {
    let mut min_y = f32::MAX;
    let mut max_y = f32::MIN;
    for p in pts {
        min_y = min_y.min(p.1);
        max_y = max_y.max(p.1);
    }
    let y0 = (min_y.floor() as i32).max(0);
    let y1 = (max_y.ceil() as i32).min(WIN_H as i32 - 1);
    for y in y0..=y1 {
        let fy = y as f32 + 0.5;
        let mut xs = [0.0f32; 2];
        let mut n = 0;
        for i in 0..4 {
            let (ax, ay) = pts[i];
            let (bx, by) = pts[(i + 1) % 4];
            if (ay <= fy && fy < by) || (by <= fy && fy < ay) {
                if n < 2 {
                    xs[n] = ax + (fy - ay) * (bx - ax) / (by - ay);
                    n += 1;
                }
            }
        }
        if n == 2 {
            let mut a = xs[0] as i32;
            let mut b = xs[1] as i32;
            if a > b {
                std::mem::swap(&mut a, &mut b);
            }
            let a = a.max(0);
            let b = b.min(WIN_W as i32 - 1);
            let row = (y as usize) * WIN_W;
            for x in a..=b {
                let idx = row + x as usize;
                buf[idx] = if alpha >= 255 {
                    color
                } else {
                    blend(buf[idx], color, alpha)
                };
            }
        }
    }
}

/// Screen center of a block's top face at the given count (1 = ground
/// level block top).
fn project_center(x: i32, z: i32, count: i32) -> (f32, f32) {
    let cx = ORIGIN_X + (x - z) as f32 * (TILE_W as f32 * 0.5);
    let cy = ORIGIN_Y + (x + z) as f32 * (TILE_H as f32 * 0.5) - count as f32 * TILE_H as f32;
    (cx, cy)
}

/// The top-face diamond at a face center.
fn top_face(cx: f32, cy: f32) -> [(f32, f32); 4] {
    let tw = TILE_W as f32 * 0.5;
    let th = TILE_H as f32 * 0.5;
    [
        (cx, cy - th),
        (cx + tw, cy),
        (cx, cy + th),
        (cx - tw, cy),
    ]
}

/// South face (faces the camera): the S edge of the top diamond extruded
/// down one block.
fn south_face(cx: f32, cy: f32) -> [(f32, f32); 4] {
    let tw = TILE_W as f32 * 0.5;
    let th = TILE_H as f32 * 0.5;
    let h = TILE_H as f32;
    [
        (cx - tw, cy),
        (cx, cy + th),
        (cx, cy + th + h),
        (cx - tw, cy + h),
    ]
}

/// East face: the E edge of the top diamond extruded down one block.
fn east_face(cx: f32, cy: f32) -> [(f32, f32); 4] {
    let tw = TILE_W as f32 * 0.5;
    let th = TILE_H as f32 * 0.5;
    let h = TILE_H as f32;
    [
        (cx + tw, cy),
        (cx, cy + th),
        (cx, cy + th + h),
        (cx + tw, cy + h),
    ]
}

/// West face: the W edge of the top diamond extruded down one block.
///
/// NOT drawn: in this 2:1 projection (view axis (1,1,1)) the west and
/// north faces are backfaces whose screen regions lie inside the block's
/// own top/south/east faces.
fn fill_sky(buf: &mut [u32]) {
    let top = Color(96, 150, 210);
    let bottom = Color(212, 228, 240);
    for y in 0..WIN_H {
        let t = y as f32 / (WIN_H - 1) as f32;
        let c = Color(
            (top.0 as f32 + (bottom.0 as f32 - top.0 as f32) * t) as u8,
            (top.1 as f32 + (bottom.1 as f32 - top.1 as f32) * t) as u8,
            (top.2 as f32 + (bottom.2 as f32 - top.2 as f32) * t) as u8,
        );
        let color = pack(c);
        for x in 0..WIN_W {
            buf[y * WIN_W + x] = color;
        }
    }
}

/// Renders the world with the painter's algorithm: columns in ascending
/// (x + z), so nearer columns overdraw farther ones.
fn render(buf: &mut [u32], world: &World, hover: Option<(i32, i32)>) {
    fill_sky(buf);
    for s in 0..WORLD_W + WORLD_D - 1 {
        let x0 = (s - (WORLD_D - 1)).max(0);
        let x1 = s.min(WORLD_W - 1);
        for x in x0..=x1 {
            let z = s - x;
            let top = world.top_at(x, z);
            if top == 0 {
                continue;
            }
            // Neighbor top counts; out-of-bounds counts as empty.
            let south = world.top_at(x, z + 1);
            let east = world.top_at(x + 1, z);
            for slot in 0..top {
                let id = world.get(x, z, slot);
                if id == AIR {
                    continue;
                }
                // A block is fully hidden when its top is covered and both
                // the south and east neighbors are at least as tall.
                let exposed = slot == top - 1 || slot >= south || slot >= east;
                if !exposed {
                    continue;
                }
                let (cx, cy) = project_center(x, z, slot + 1);
                if id == WATER {
                    if slot == top - 1 {
                        fill_quad(buf, &top_face(cx, cy), pack(palette(id).0), 255);
                    }
                    continue;
                }
                let (top_c, south_c, east_c, _, _) = palette(id);
                if slot == top - 1 {
                    fill_quad(buf, &top_face(cx, cy), pack(top_c), 255);
                }
                // The south face is covered by the south neighbor's body
                // when the neighbor is at least as tall; same for east.
                if slot >= south {
                    fill_quad(buf, &south_face(cx, cy), pack(south_c), 255);
                }
                if slot >= east {
                    fill_quad(buf, &east_face(cx, cy), pack(east_c), 255);
                }
            }
        }
    }
    if let Some((hx, hz)) = hover {
        let top = world.top_at(hx, hz);
        if top > 0 {
            let (cx, cy) = project_center(hx, hz, top);
            fill_quad(buf, &top_face(cx, cy), pack(Color(255, 236, 120)), 110);
        }
    }
}

/// The column whose top face contains the point, choosing the nearest
/// (highest x + z) when several overlap on screen.
fn pick_tile(world: &World, mx: f32, my: f32) -> Option<(i32, i32)> {
    let mut picked = None;
    for s in 0..WORLD_W + WORLD_D - 1 {
        let x0 = (s - (WORLD_D - 1)).max(0);
        let x1 = s.min(WORLD_W - 1);
        for x in x0..=x1 {
            let z = s - x;
            let top = world.top_at(x, z);
            if top == 0 {
                continue;
            }
            let (cx, cy) = project_center(x, z, top);
            if point_in_top_face(cx, cy, mx, my) {
                picked = Some((x, z));
            }
        }
    }
    picked
}

/// Half-plane test for the convex top-face diamond (ccw winding).
fn point_in_top_face(cx: f32, cy: f32, px: f32, py: f32) -> bool {
    let pts = top_face(cx, cy);
    for i in 0..4 {
        let (ax, ay) = pts[i];
        let (bx, by) = pts[(i + 1) % 4];
        let cross = (bx - ax) * (py - ay) - (by - ay) * (px - ax);
        if cross < 0.0 {
            return false;
        }
    }
    true
}

/// Writes the RGBA backbuffer as a PPM P6 image (debugging/headless use).
fn save_ppm(buf: &[u32], path: &str) {
    let mut bytes = Vec::with_capacity(WIN_W * WIN_H * 3);
    for px in buf {
        bytes.push((px & 0xFF) as u8);
        bytes.push(((px >> 8) & 0xFF) as u8);
        bytes.push(((px >> 16) & 0xFF) as u8);
    }
    let header = format!("P6\n{WIN_W} {WIN_H}\n255\n");
    std::fs::write(path, [header.as_bytes(), &bytes].concat()).expect("screenshot write");
}

fn brush_for_key(key: u32) -> Option<u8> {
    // SDL keycodes for '1'-'7' are the ASCII values.
    const K1: u32 = b'1' as u32;
    const K2: u32 = b'2' as u32;
    const K3: u32 = b'3' as u32;
    const K4: u32 = b'4' as u32;
    const K5: u32 = b'5' as u32;
    const K6: u32 = b'6' as u32;
    const K7: u32 = b'7' as u32;
    match key {
        K1 => Some(GRASS),
        K2 => Some(DIRT),
        K3 => Some(STONE),
        K4 => Some(WOOD),
        K5 => Some(LEAF),
        K6 => Some(WATER),
        K7 => Some(BRICK),
        _ => None,
    }
}

fn brush_name(brush: u8) -> &'static str {
    match brush {
        GRASS => "grass",
        DIRT => "dirt",
        STONE => "stone",
        WOOD => "wood",
        LEAF => "leaf",
        WATER => "water",
        BRICK => "brick",
        _ => "?",
    }
}

fn main() {
    // Optional `--frames N`: exit after N frames (headless runs).
    // Optional `--screenshot PATH`: write frame 1 as a PPM image.
    let mut max_frames: u64 = 0;
    let mut screenshot: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--frames" => {
                if let Some(value) = args.next().and_then(|v| v.parse().ok()) {
                    max_frames = value;
                }
            }
            "--screenshot" => {
                if let Some(path) = args.next() {
                    screenshot = Some(path);
                }
            }
            _ => {}
        }
    }

    println!("voxel-city: left-click place, right-click remove, keys 1-7 brush, ESC quit");

    unsafe {
        if !SDL_Init(SDL_INIT_VIDEO) {
            panic!("SDL_Init failed: {}", last_error());
        }

        let title = CString::new("tong + SDL3: voxel-city").expect("title");
        let window = SDL_CreateWindow(
            title.as_ptr(),
            WIN_W as c_int,
            WIN_H as c_int,
            SDL_WINDOW_RESIZABLE,
        );
        if window.is_null() {
            panic!("SDL_CreateWindow failed: {}", last_error());
        }
        let renderer = SDL_CreateRenderer(window, std::ptr::null());
        if renderer.is_null() {
            panic!("SDL_CreateRenderer failed: {}", last_error());
        }
        let texture = SDL_CreateTexture(
            renderer,
            SDL_PIXELFORMAT_RGBA8888,
            SDL_TEXTUREACCESS_STREAMING,
            WIN_W as c_int,
            WIN_H as c_int,
        );
        if texture.is_null() {
            panic!("SDL_CreateTexture failed: {}", last_error());
        }

        let mut world = World::new();
        let mut brush = GRASS;
        let mut frame: u64 = 0;
        let mut quit = false;

        while !quit {
            let mut event = SDL_Event::new();
            while SDL_PollEvent(&mut event) {
                match event.type_() {
                    SDL_EVENT_QUIT | SDL_EVENT_WINDOW_CLOSE_REQUESTED => quit = true,
                    SDL_EVENT_KEY_DOWN => {
                        let key = event.key();
                        if key.key == SDLK_ESCAPE {
                            quit = true;
                        } else if let Some(next) = brush_for_key(key.key) {
                            brush = next;
                            println!("brush: {}", brush_name(brush));
                        }
                    }
                    SDL_EVENT_MOUSE_BUTTON_DOWN => {
                        let button = event.button();
                        let mut mx = 0.0f32;
                        let mut my = 0.0f32;
                        SDL_GetMouseState(&mut mx, &mut my);
                        if let Some((x, z)) = pick_tile(&world, mx, my) {
                            if button.button == SDL_BUTTON_LEFT {
                                world.place(x, z, brush);
                            } else if button.button == SDL_BUTTON_RIGHT {
                                world.remove(x, z);
                            }
                        }
                    }
                    _ => {}
                }
            }

            let mut mx = 0.0f32;
            let mut my = 0.0f32;
            SDL_GetMouseState(&mut mx, &mut my);
            let hover = pick_tile(&world, mx, my);

            let mut buf = vec![0u32; WIN_W * WIN_H];
            render(&mut buf, &world, hover);
            if frame == 0 {
                if let Some(path) = &screenshot {
                    save_ppm(&buf, path);
                    println!("wrote screenshot to {path}");
                }
            }
            SDL_UpdateTexture(
                texture,
                std::ptr::null(),
                buf.as_ptr() as *const c_void,
                (WIN_W * 4) as c_int,
            );
            SDL_RenderTexture(renderer, texture, std::ptr::null(), std::ptr::null());
            SDL_RenderPresent(renderer);

            frame += 1;
            if max_frames > 0 && frame >= max_frames {
                quit = true;
            }
            SDL_Delay(16);
        }

        SDL_DestroyTexture(texture);
        SDL_DestroyRenderer(renderer);
        SDL_DestroyWindow(window);
        SDL_Quit();
        println!("voxel-city finished cleanly");
    }
}
