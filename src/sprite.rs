//! Procedural pixel-art black hole, rendered at a native 32×32 grid.
//!
//! Everything is quantised to a handful of colours and integer pixels so it
//! still reads as pixel art after nearest-neighbour scaling.

pub const SIZE: usize = 32;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mood {
    Idle,
    /// Something is being dragged over the dot.
    Hungry,
    /// Just swallowed something / indexing.
    Digesting,
    /// Finished indexing.
    Satisfied,
    /// Something went wrong.
    Upset,
    /// Search panel is open.
    Listening,
}

#[derive(Clone, Copy)]
struct Rgb(u8, u8, u8);

const CORE: Rgb = Rgb(0, 0, 0);
const RING: [Rgb; 5] = [
    Rgb(38, 12, 64),    // deepest violet
    Rgb(96, 28, 128),   // violet
    Rgb(200, 72, 60),   // ember
    Rgb(255, 160, 64),  // orange
    Rgb(255, 236, 200), // hot white
];
const RING_UPSET: [Rgb; 5] = [
    Rgb(60, 6, 6),
    Rgb(120, 12, 12),
    Rgb(200, 30, 30),
    Rgb(255, 80, 60),
    Rgb(255, 210, 200),
];
const RING_LISTENING: [Rgb; 5] = [
    Rgb(8, 48, 36),     // deep teal
    Rgb(20, 104, 64),   // green
    Rgb(60, 180, 96),   // bright green
    Rgb(140, 240, 150), // mint
    Rgb(230, 255, 230), // hot white-green
];
const GLOW: Rgb = Rgb(120, 60, 180);
const GLOW_LISTENING: Rgb = Rgb(60, 160, 110);

fn hash2(x: i32, y: i32, salt: u32) -> u32 {
    let mut h = (x as u32).wrapping_mul(0x9E37_79B9) ^ (y as u32).wrapping_mul(0x85EB_CA6B) ^ salt;
    h ^= h >> 15;
    h = h.wrapping_mul(0x2C1B_3C6D);
    h ^= h >> 12;
    h
}

fn put(buf: &mut [u32], x: usize, y: usize, c: Rgb, a: u8) {
    // Premultiplied BGRA as required by UpdateLayeredWindow.
    let pm = |v: u8| ((v as u32 * a as u32) / 255) as u32;
    buf[y * SIZE + x] = (a as u32) << 24 | pm(c.0) << 16 | pm(c.1) << 8 | pm(c.2);
}

/// Ring rotation per tick for a mood; the caller accumulates this into `phase`
/// so speed changes don't make the ring jump.
pub fn speed(mood: Mood) -> f32 {
    match mood {
        Mood::Idle => 0.5,
        Mood::Hungry => 2.0,
        Mood::Digesting => 4.0,
        Mood::Satisfied => 1.5,
        Mood::Upset => 0.5,
        Mood::Listening => 1.0,
    }
}

/// Render one frame into a 32×32 premultiplied BGRA buffer.
/// `frame` drives the twinkles, `phase` the ring rotation.
pub fn render(buf: &mut [u32], frame: u32, phase: f32, mood: Mood) {
    debug_assert!(buf.len() >= SIZE * SIZE);
    buf.iter_mut().for_each(|p| *p = 0);

    let (grow, bright, palette, glow) = match mood {
        Mood::Idle => (0.0, 0.0, &RING, GLOW),
        Mood::Hungry => (1.0, 0.6, &RING, GLOW),
        Mood::Digesting => (0.0, 0.3, &RING, GLOW),
        Mood::Satisfied => (0.5, 1.0, &RING, GLOW),
        Mood::Upset => (0.0, 0.4, &RING_UPSET, GLOW),
        Mood::Listening => (0.0, 0.5, &RING_LISTENING, GLOW_LISTENING),
    };
    let c = (SIZE as f32 - 1.0) / 2.0;
    let core_r = 6.4;
    let ring_in = core_r;
    let ring_out = 9.6 + grow;
    let glow_out = 12.5 + grow;

    for y in 0..SIZE {
        for x in 0..SIZE {
            let dx = x as f32 - c;
            let dy = y as f32 - c;
            // Squash vertically a touch so the ring reads as a tilted disc.
            let r = (dx * dx + (dy * 1.25) * (dy * 1.25)).sqrt();
            let ang = dy.atan2(dx);

            if r < core_r {
                put(buf, x, y, CORE, 255);
            } else if r < ring_out {
                // Brightness runs around the ring with a couple of hot spots that rotate.
                let t = (r - ring_in) / (ring_out - ring_in); // 0 inner .. 1 outer
                let wave = 0.5 + 0.5 * (ang * 2.0 + phase).sin();
                let wave2 = 0.5 + 0.5 * (ang * 5.0 - phase * 1.7).sin();
                let inner_heat = 1.0 - t; // hotter near the horizon
                let v = (0.25 + 0.55 * wave + 0.2 * wave2) * (0.55 + 0.45 * inner_heat) + bright * 0.35;
                let idx = ((v * 4.0).round() as usize).min(4);
                put(buf, x, y, palette[idx], 255);
            } else if r < glow_out && mood != Mood::Digesting {
                // Sparse specks in the halo; a few of them twinkle.
                let h = hash2(x as i32, y as i32, 7);
                let density = (h % 100) as f32 / 100.0;
                let falloff = 1.0 - (r - ring_out) / (glow_out - ring_out);
                // Twinkles re-roll about once a second rather than every few ticks.
                let twinkle = hash2(x as i32, y as i32, frame / 11) % 13 == 0;
                if density < 0.22 * falloff + bright * 0.15 || twinkle {
                    let a = if twinkle { 200 } else { (110.0 * falloff) as u8 + 40 };
                    let col = if twinkle { palette[4] } else { glow };
                    put(buf, x, y, col, a);
                }
            }
        }
    }

    // A pixel or two spiralling into the core while something hovers.
    if mood == Mood::Hungry {
        for k in 0..2 {
            let t = ((frame + k * 7) % 14) as f32 / 14.0;
            let a = phase * 2.0 + k as f32 * 3.1;
            let rr = ring_out + 2.5 - t * (ring_out + 2.5 - core_r + 1.0);
            let px = (c + rr * a.cos()).round() as i32;
            let py = (c + rr * a.sin() * 0.8).round() as i32;
            if (0..SIZE as i32).contains(&px) && (0..SIZE as i32).contains(&py) {
                put(buf, px as usize, py as usize, RING[4], 255);
            }
        }
    }

    // While digesting, the halo specks stream into the core: each particle
    // spawns at a random angle and radius out in the halo, falls in on its own
    // schedule, and respawns somewhere else.
    if mood == Mood::Digesting {
        const PARTICLES: u32 = 14;
        for k in 0..PARTICLES {
            let offset = hash2(k as i32, 0, 101) % 40;
            let period = 12 + hash2(k as i32, 1, 103) % 10; // 12..21 ticks to fall
            let life = frame + offset;
            let cycle = life / period;
            let t = (life % period) as f32 / period as f32; // 0 = spawn .. 1 = swallowed
            let h = hash2(k as i32, cycle as i32, 107);
            let a0 = (h % 628) as f32 / 100.0;
            let r0 = glow_out + 1.5 + ((h >> 10) % 4) as f32;
            // Ease in: slow start, then it accelerates and curves as it nears the horizon.
            let e = t * t;
            let rr = r0 - e * (r0 - core_r + 0.5);
            let a = a0 + e * 1.6;
            let px = (c + rr * a.cos()).round() as i32;
            let py = (c + rr * a.sin() * 0.8).round() as i32;
            if (0..SIZE as i32).contains(&px) && (0..SIZE as i32).contains(&py) {
                let col = if t < 0.35 { GLOW } else { RING[4] };
                let alpha = if t < 0.15 { 120 } else { 255 };
                put(buf, px as usize, py as usize, col, alpha);
            }
        }
    }
}
