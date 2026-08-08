//! voxel-city: a tiny isometric voxel city builder.
//!
//! Built on the shared `sdl3-sys` bindings from `examples/03-sdl3`, which
//! `Tong.toml` imports as an external crate root. A deterministic seed
//! town — road grid, city blocks, parks, a pond, rim trees — is rendered
//! with a painter's-algorithm 2:1 isometric projection into a streaming
//! RGBA8888 texture; the mouse places, carves, and removes blocks.
//!
//! Controls:
//! - left click: place the brush on the hovered column (grass/dirt/stone/
//!   brick/wood/leaf stack; water and road carve the column flat)
//! - right click: remove the top block of the hovered column
//! - keys 1-8: choose the brush
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
const BRICK: u8 = 4;
const WOOD: u8 = 5;
const LEAF: u8 = 6;
const WATER: u8 = 7;
const ROAD: u8 = 8;
const ROOF: u8 = 9;

/// Roads run every 8 columns (at `x % 8 == 4`, same for z), splitting the
/// map into 7x7 city blocks.
const ROAD_EVERY: i32 = 8;
const ROAD_AT: i32 = 4;

const TILE_W: i32 = 24;
const TILE_H: i32 = 12;
const WIN_W: usize = 960;
const WIN_H: usize = 640;
const ORIGIN_X: f32 = 480.0;
const ORIGIN_Y: f32 = 150.0;

#[derive(Clone, Copy)]
struct Color(u8, u8, u8);

/// Per-pixel surface texture for a block type.
#[derive(Clone, Copy)]
enum Tex {
    Flat,
    /// Subtle brightness noise, ±delta.
    Noise(i32),
    /// Brick courses with staggered vertical joints.
    Brick,
    /// Horizontal plank lines.
    Planks,
    /// Two-tone leaf dither.
    Leaf,
    /// Water shimmer bands.
    Shimmer,
}

/// A block's look: top/south/east face colors plus surface texture.
struct Style {
    top: Color,
    south: Color,
    east: Color,
    tex: Tex,
}

fn style(id: u8) -> Style {
    match id {
        GRASS => Style {
            top: Color(104, 172, 84),
            south: Color(78, 140, 64),
            east: Color(68, 122, 56),
            tex: Tex::Noise(6),
        },
        DIRT => Style {
            top: Color(150, 110, 66),
            south: Color(120, 88, 54),
            east: Color(106, 78, 48),
            tex: Tex::Noise(9),
        },
        STONE => Style {
            top: Color(146, 148, 152),
            south: Color(120, 122, 126),
            east: Color(108, 110, 114),
            tex: Tex::Noise(4),
        },
        BRICK => Style {
            top: Color(168, 98, 72),
            south: Color(142, 82, 60),
            east: Color(126, 72, 53),
            tex: Tex::Brick,
        },
        WOOD => Style {
            top: Color(150, 112, 70),
            south: Color(124, 92, 58),
            east: Color(110, 82, 52),
            tex: Tex::Planks,
        },
        LEAF => Style {
            top: Color(64, 146, 60),
            south: Color(52, 118, 49),
            east: Color(46, 104, 44),
            tex: Tex::Leaf,
        },
        WATER => Style {
            top: Color(58, 132, 212),
            south: Color(58, 132, 212),
            east: Color(58, 132, 212),
            tex: Tex::Shimmer,
        },
        ROAD => Style {
            top: Color(56, 60, 68),
            south: Color(56, 60, 68),
            east: Color(56, 60, 68),
            tex: Tex::Noise(3),
        },
        ROOF => Style {
            top: Color(106, 118, 140),
            south: Color(90, 101, 122),
            east: Color(81, 91, 110),
            tex: Tex::Noise(6),
        },
        _ => Style {
            top: Color(255, 0, 255),
            south: Color(255, 0, 255),
            east: Color(255, 0, 255),
            tex: Tex::Flat,
        },
    }
}

/// Adds a signed delta to each channel, clamped.
fn shade(c: Color, delta: i32) -> Color {
    let f = |v: u8| (v as i32 + delta).clamp(0, 255) as u8;
    Color(f(c.0), f(c.1), f(c.2))
}

struct World {
    blocks: Vec<u8>,
    /// Top count per column: number of blocks (0 = empty column).
    top: Vec<u8>,
    /// City statistics for the startup banner.
    buildings: u32,
    trees: u32,
    road_tiles: u32,
}

impl World {
    fn new() -> Self {
        let mut world = World {
            blocks: vec![AIR; WORLD_W as usize * WORLD_D as usize * WORLD_H],
            top: vec![0; WORLD_W as usize * WORLD_D as usize],
            buildings: 0,
            trees: 0,
            road_tiles: 0,
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

    /// Clears the column and sets `id` at slot 0 (leveling brushes).
    fn flatten(&mut self, x: i32, z: i32, id: u8) {
        let t = self.top_at(x, z);
        for y in 0..t {
            self.set(x, z, y, AIR);
        }
        self.set(x, z, 0, id);
    }

    /// Stacks the brush on top of the column; water and road carve flat.
    fn place(&mut self, x: i32, z: i32, brush: u8) {
        match brush {
            WATER => self.flatten(x, z, WATER),
            ROAD => {
                self.flatten(x, z, DIRT);
                self.set(x, z, 1, ROAD);
            }
            _ => {
                let t = self.top_at(x, z);
                if t < WORLD_H as i32 {
                    self.set(x, z, t, brush);
                }
            }
        }
    }

    fn remove(&mut self, x: i32, z: i32) {
        let t = self.top_at(x, z);
        if t > 0 {
            self.set(x, z, t - 1, AIR);
        }
    }

    /// Deterministic seed town: gentle terrain, a road grid, city blocks
    /// with parks and buildings, and a ring of trees at the map edge.
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

        // Road grid: a flat strip every 8 columns, with a grass shoulder
        // on each side so the strip reads continuously (the taller
        // terrain between blocks would otherwise hide it).
        for x in 0..WORLD_W {
            for z in 0..WORLD_D {
                if x % ROAD_EVERY == ROAD_AT || z % ROAD_EVERY == ROAD_AT {
                    self.flatten(x, z, DIRT);
                    self.set(x, z, 1, ROAD);
                    self.road_tiles += 1;
                    for (sx, sz) in [(x - 1, z), (x + 1, z), (x, z - 1), (x, z + 1)] {
                        if sx >= 0 && sz >= 0 && sx < WORLD_W && sz < WORLD_D {
                            self.flatten(sx, sz, DIRT);
                            self.set(sx, sz, 1, GRASS);
                        }
                    }
                }
            }
        }

        // City blocks between the roads (7x7 columns each).
        for bx in 0..4 {
            for bz in 0..4 {
                let cx = 8 * bx + 8;
                let cz = 8 * bz + 8;
                if bx == 3 && bz == 3 {
                    // The lake sits in the SE-most park: anything taller in
                    // front (SE) would occlude it in this steep view.
                    self.park(cx, cz, true);
                    continue;
                }
                match hash2(bx, bz, 7) % 10 {
                    0 | 1 => self.park(cx, cz, false),
                    _ => {
                        let dist = (bx as i32 - 2).abs() + (bz as i32 - 2).abs();
                        self.city_block(cx, cz, dist);
                    }
                }
            }
        }

        // Rim trees along the map edge.
        for (tx, tz) in [
            (2, 2),
            (2, 36),
            (36, 2),
            (1, 10),
            (1, 26),
            (10, 1),
            (26, 1),
            (38, 10),
            (38, 26),
            (10, 38),
            (26, 38),
            (37, 33),
        ] {
            self.tree(tx, tz);
        }
    }

    fn ground_height(x: i32, z: i32) -> i32 {
        1 + (hash2(x / 3, z / 3, 1) % 2) as i32 + (hash2(x / 7, z / 7, 2) % 2) as i32
    }

    /// A park block: level lawn, a few trees, optionally the pond.
    fn park(&mut self, cx: i32, cz: i32, with_pond: bool) {
        for x in cx - 3..=cx + 3 {
            for z in cz - 3..=cz + 3 {
                self.flatten(x, z, GRASS);
            }
        }
        if with_pond {
            for x in cx - 3..=cx + 3 {
                for z in cz - 3..=cz + 3 {
                    let dx = x - cx;
                    let dz = z - cz;
                    let d2 = dx * dx + dz * dz;
                    if d2 <= 9 {
                        self.flatten(x, z, WATER);
                    }
                }
            }
        }
        self.tree(cx - 3, cz - 3);
        self.tree(cx + 3, cz - 3);
        self.tree(cx - 3, cz + 3);
        if with_pond {
            // The SE corner of the pond park sits on the camera's line of
            // sight to the lake center; plant that tree on the east edge
            // instead so the water stays visible.
            self.tree(cx + 3, cz - 1);
        } else {
            self.tree(cx + 3, cz + 3);
        }
    }

    /// A city block: one or two buildings on a leveled base, taller
    /// towards the map center.
    fn city_block(&mut self, cx: i32, cz: i32, dist: i32) {
        let h = hash2(cx, cz, 11);
        let count = 1 + (h % 2) as i32;
        for i in 0..count {
            let r = hash2(cx + i * 97, cz + i * 131, 13);
            let w = 3 + (r % 3) as i32;
            let d = 3 + ((r >> 4) % 3) as i32;
            let height = 2 + ((r >> 8) % 3) as i32 + (4 - dist).max(0);
            let material = if (r >> 12) & 1 == 0 { BRICK } else { STONE };
            let x0 = cx - 3 + ((r >> 16) % (8 - w) as u32) as i32;
            let z0 = cz - 3 + ((r >> 20) % (8 - d) as u32) as i32;
            self.building(x0, z0, w, d, height, material);
        }
    }

    fn building(&mut self, x0: i32, z0: i32, w: i32, d: i32, h: i32, material: u8) {
        // Level base so the building sits on flat ground.
        for x in x0..x0 + w {
            for z in z0..z0 + d {
                self.flatten(x, z, GRASS);
            }
        }
        for x in x0..x0 + w {
            for z in z0..z0 + d {
                for y in 0..h {
                    let id = if y + 1 == h { ROOF } else { material };
                    self.set(x, z, 1 + y, id);
                }
            }
        }
        // Taller buildings sometimes get a rooftop box (water tower /
        // mechanical room) for silhouette variety.
        if h >= 4 && hash2(x0 * 7 + z0 * 13, 17, 5) % 3 == 0 {
            let cx = x0 + (hash2(x0 * 7 + z0 * 13, 19, 5) % 2) as i32 * (w - 1);
            let cz = z0 + (hash2(x0 * 7 + z0 * 13, 21, 5) % 2) as i32 * (d - 1);
            let top = if hash2(x0 * 7 + z0 * 13, 23, 5) & 1 == 0 {
                STONE
            } else {
                WOOD
            };
            self.set(cx, cz, 1 + h, top);
        }
        self.buildings += 1;
    }

    fn tree(&mut self, tx: i32, tz: i32) {
        // Don't plant in the pond: a canopy on a water column would hide
        // the water surface and leave a hole.
        if self.get(tx, tz, 0) == WATER {
            return;
        }
        let g = self.top_at(tx, tz);
        self.set(tx, tz, g, WOOD);
        self.set(tx, tz, g + 1, WOOD);
        // Fully solid 3x3 canopy (three leaf slabs, trunk column capped
        // with leaf): any air gap inside would leave a sky hole, since
        // interior faces are culled against taller neighbors.
        for x in tx - 1..=tx + 1 {
            for z in tz - 1..=tz + 1 {
                if self.get(x, z, 0) == WATER {
                    continue;
                }
                self.set(x, z, g + 2, LEAF);
                self.set(x, z, g + 1, LEAF);
                if x != tx || z != tz {
                    self.set(x, z, g, LEAF);
                }
            }
        }
        self.trees += 1;
    }
}

/// Integer hash for the deterministic terrain and layout.
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

/// Fills a convex quad with a scanline sweep, applying the block's
/// per-pixel texture and an optional vertical side gradient.
#[allow(clippy::too_many_arguments)]
fn fill_quad(
    buf: &mut [u32],
    pts: &[(f32, f32); 4],
    base: Color,
    alpha: u32,
    tex: Tex,
    seed: u32,
    grad: Option<(f32, f32)>,
) {
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
                let mut c = base;
                match tex {
                    Tex::Flat => {}
                    Tex::Noise(delta) => {
                        let n = (hash2(x, y, seed as i32) % (2 * delta as u32 + 1)) as i32 - delta;
                        c = shade(c, n);
                    }
                    Tex::Brick => {
                        let row_no = y / 5;
                        let joint = (x + (row_no & 1) * 5) % 10 < 2;
                        let bed = y % 5 < 1;
                        if joint || bed {
                            c = shade(c, -28);
                        }
                    }
                    Tex::Planks => {
                        if y % 6 < 1 {
                            c = shade(c, -14);
                        }
                    }
                    Tex::Leaf => {
                        let n = hash2(x, y, seed as i32);
                        c = shade(c, if n & 1 == 0 { -9 } else { 5 });
                    }
                    Tex::Shimmer => {
                        if (y / 4) & 1 == 0 {
                            c = shade(c, 7);
                        }
                        c = shade(c, (hash2(x, y, seed as i32) % 5) as i32 - 2);
                    }
                }
                if let Some((top_y, bottom_y)) = grad {
                    let t = ((fy - top_y) / (bottom_y - top_y).max(1.0)).clamp(0.0, 1.0);
                    c = shade(c, -(t * 9.0) as i32);
                }
                let idx = row + x as usize;
                buf[idx] = if alpha >= 255 {
                    pack(c)
                } else {
                    blend(buf[idx], pack(c), alpha)
                };
            }
        }
    }
}

/// Fills a disc (sun, clouds) with optional alpha.
fn fill_circle(buf: &mut [u32], cx: f32, cy: f32, r: f32, color: Color, alpha: u32) {
    let x0 = ((cx - r).floor() as i32).max(0);
    let x1 = ((cx + r).ceil() as i32).min(WIN_W as i32 - 1);
    let y0 = ((cy - r).floor() as i32).max(0);
    let y1 = ((cy + r).ceil() as i32).min(WIN_H as i32 - 1);
    let r2 = r * r;
    for y in y0..=y1 {
        for x in x0..=x1 {
            let dx = x as f32 - cx;
            let dy = y as f32 - cy;
            if dx * dx + dy * dy <= r2 {
                let idx = (y as usize) * WIN_W + x as usize;
                buf[idx] = if alpha >= 255 {
                    pack(color)
                } else {
                    blend(buf[idx], pack(color), alpha)
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

/// The top-face diamond scaled around its center — used for the dark
/// bevel rim that gives tiles their definition.
fn scaled_top_face(cx: f32, cy: f32, factor: f32) -> [(f32, f32); 4] {
    let pts = top_face(cx, cy);
    let mut out = [(0.0f32, 0.0f32); 4];
    for (i, (x, y)) in pts.iter().enumerate() {
        out[i] = (cx + (x - cx) * factor, cy + (y - cy) * factor);
    }
    out
}

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
    // Sun with a soft glow, and two clouds.
    fill_circle(buf, 140.0, 86.0, 52.0, Color(255, 242, 200), 60);
    fill_circle(buf, 140.0, 86.0, 36.0, Color(255, 236, 170), 255);
    for (cx, cy, r) in [
        (330.0, 112.0, 24.0),
        (356.0, 102.0, 18.0),
        (310.0, 98.0, 16.0),
        (720.0, 70.0, 20.0),
        (742.0, 62.0, 15.0),
        (702.0, 58.0, 13.0),
    ] {
        fill_circle(buf, cx, cy, r, Color(244, 248, 252), 255);
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
                let st = style(id);
                if id == WATER || id == ROAD {
                    // Flat surfaces: top face only, with a bevel rim.
                    if slot == top - 1 {
                        let seed = hash2(x * 7 + z * 13 + slot * 31, 3, 5);
                        fill_quad(
                            buf,
                            &scaled_top_face(cx, cy, 1.08),
                            shade(st.top, -26),
                            255,
                            Tex::Flat,
                            0,
                            None,
                        );
                        fill_quad(buf, &top_face(cx, cy), st.top, 255, st.tex, seed, None);
                    }
                    continue;
                }
                let seed = hash2(x * 7 + z * 13 + slot * 31, 3, 5);
                if slot == top - 1 {
                    // Beveled top face.
                    fill_quad(
                        buf,
                        &scaled_top_face(cx, cy, 1.08),
                        shade(st.top, -26),
                        255,
                        Tex::Flat,
                        0,
                        None,
                    );
                    fill_quad(buf, &top_face(cx, cy), st.top, 255, st.tex, seed, None);
                }
                // The south face is covered by the south neighbor's body
                // when the neighbor is at least as tall; same for east.
                if slot >= south {
                    fill_quad(
                        buf,
                        &south_face(cx, cy),
                        st.south,
                        255,
                        st.tex,
                        seed,
                        Some((cy, cy + TILE_H as f32)),
                    );
                }
                if slot >= east {
                    fill_quad(
                        buf,
                        &east_face(cx, cy),
                        st.east,
                        255,
                        st.tex,
                        seed,
                        Some((cy, cy + TILE_H as f32)),
                    );
                }
            }
        }
    }
    if let Some((hx, hz)) = hover {
        let top = world.top_at(hx, hz);
        if top > 0 {
            let (cx, cy) = project_center(hx, hz, top);
            fill_quad(
                buf,
                &scaled_top_face(cx, cy, 1.1),
                Color(255, 236, 120),
                170,
                Tex::Flat,
                0,
                None,
            );
            fill_quad(
                buf,
                &top_face(cx, cy),
                Color(255, 236, 120),
                110,
                Tex::Flat,
                0,
                None,
            );
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

fn brush_for_key(key: u32) -> Option<u8> {
    // SDL keycodes for '1'-'8' are the ASCII values.
    const K1: u32 = b'1' as u32;
    const K2: u32 = b'2' as u32;
    const K3: u32 = b'3' as u32;
    const K4: u32 = b'4' as u32;
    const K5: u32 = b'5' as u32;
    const K6: u32 = b'6' as u32;
    const K7: u32 = b'7' as u32;
    const K8: u32 = b'8' as u32;
    match key {
        K1 => Some(GRASS),
        K2 => Some(DIRT),
        K3 => Some(STONE),
        K4 => Some(BRICK),
        K5 => Some(WOOD),
        K6 => Some(LEAF),
        K7 => Some(WATER),
        K8 => Some(ROAD),
        _ => None,
    }
}

fn brush_name(brush: u8) -> &'static str {
    match brush {
        GRASS => "grass",
        DIRT => "dirt",
        STONE => "stone",
        BRICK => "brick",
        WOOD => "wood",
        LEAF => "leaf",
        WATER => "water",
        ROAD => "road",
        _ => "?",
    }
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

    println!(
        "voxel-city: left-click place, right-click remove, keys 1-8 brush, ESC quit"
    );

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
        println!(
            "seed town: {} buildings, {} trees, {} road tiles",
            world.buildings, world.trees, world.road_tiles
        );
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
                            let title =
                                CString::new(format!("voxel-city — {}", brush_name(brush)))
                                    .expect("title");
                            SDL_SetWindowTitle(window, title.as_ptr());
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
