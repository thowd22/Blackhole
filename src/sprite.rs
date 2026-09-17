//! Procedural pixel-art black hole, rendered at a native 32×32 grid.
//!
//! Everything is quantised to a handful of colours and integer pixels so it
//! still reads as pixel art after nearest-neighbour scaling. The specks that
//! orbit and fall in are the one exception: they live on a 128×128 "quarter-pixel"
//! grid (`render_specks`) so their motion is smooth at 20 fps while each speck
//! is still a crisp block.
//!
//! Motion is driven by wall-clock milliseconds, not frame counts, so the dot
//! looks the same whether the tick runs at the idle or the active rate.

pub const SIZE: usize = 32;
/// Side of the speck overlay: four sub-pixels per sprite pixel, so a speck can move
/// at continuous positions with per-cell coverage (it slides rather than hops).
pub const SPECK_SIZE: usize = SIZE * 4;

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
    /// The LLM is reading / reasoning; ends on the first answer token.
    Thinking,
}

/// Everything one frame needs. `prev`/`blend` describe a mood transition in
/// progress: 0 = still fully `prev`, 1 = fully `mood`; the caller eases it
/// over ~150 ms so nothing snaps.
#[derive(Clone, Copy)]
pub struct Anim {
    /// Milliseconds since the app started; drives twinkles, particles and breathing.
    pub t_ms: u32,
    /// Ring rotation, accumulated by the caller from `speed`.
    pub phase: f32,
    pub mood: Mood,
    pub prev: Mood,
    pub blend: f32,
}

impl Anim {
    /// A motionless frame of one mood (tray icon).
    pub fn still(mood: Mood) -> Self {
        Anim { t_ms: 0, phase: 0.0, mood, prev: mood, blend: 1.0 }
    }

    fn in_transition(&self) -> bool {
        self.prev != self.mood && self.blend < 1.0
    }
}

#[derive(Clone, Copy)]
struct Rgb(u8, u8, u8);

impl Rgb {
    fn lerp(self, o: Rgb, t: f32) -> Rgb {
        let m = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
        Rgb(m(self.0, o.0), m(self.1, o.1), m(self.2, o.2))
    }
}

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

/// Thinking breathes in and out over this period.
const BREATH_MS: u32 = 2600;
/// The thinking speck orbits once per second.
const ORBIT_MS: u32 = 1000;

fn hash2(x: i32, y: i32, salt: u32) -> u32 {
    let mut h = (x as u32).wrapping_mul(0x9E37_79B9) ^ (y as u32).wrapping_mul(0x85EB_CA6B) ^ salt;
    h ^= h >> 15;
    h = h.wrapping_mul(0x2C1B_3C6D);
    h ^= h >> 12;
    h
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// Premultiplied BGRA as required by UpdateLayeredWindow.
fn pixel(c: Rgb, a: u8) -> u32 {
    let pm = |v: u8| ((v as u32 * a as u32) / 255) as u32;
    (a as u32) << 24 | pm(c.0) << 16 | pm(c.1) << 8 | pm(c.2)
}

fn put(buf: &mut [u32], x: usize, y: usize, c: Rgb, a: u8) {
    buf[y * SIZE + x] = pixel(c, a);
}

/// Ring rotation per 90 ms for a mood; the caller accumulates this into `phase`
/// so speed changes don't make the ring jump.
pub fn speed(mood: Mood) -> f32 {
    match mood {
        Mood::Idle => 0.5,
        Mood::Hungry => 2.0,
        Mood::Digesting => 4.0,
        Mood::Satisfied => 1.5,
        Mood::Upset => 0.5,
        Mood::Listening => 1.0,
        Mood::Thinking => 0.8,
    }
}

/// Whether a mood keeps specks moving on the overlay (so the caller can tick
/// faster while they are on screen and drop back to idle otherwise).
pub fn has_specks(mood: Mood) -> bool {
    matches!(mood, Mood::Hungry | Mood::Digesting | Mood::Thinking)
}

/// Ring shape and colour of a mood: (grow, bright, halo, palette, glow).
/// `halo` is 1 while the sparse halo specks are drawn (they make way for the
/// particle stream while digesting).
fn look(mood: Mood, t_ms: u32) -> (f32, f32, f32, [Rgb; 5], Rgb) {
    match mood {
        Mood::Idle => (0.0, 0.0, 1.0, RING, GLOW),
        Mood::Hungry => (1.0, 0.6, 1.0, RING, GLOW),
        Mood::Digesting => (0.0, 0.3, 0.0, RING, GLOW),
        Mood::Satisfied => (0.5, 1.0, 1.0, RING, GLOW),
        Mood::Upset => (0.0, 0.4, 1.0, RING_UPSET, GLOW),
        Mood::Listening => (0.0, 0.5, 1.0, RING_LISTENING, GLOW_LISTENING),
        Mood::Thinking => {
            // Slow breathing: the ring swells and brightens together.
            let b = 0.5 - 0.5 * ((t_ms % BREATH_MS) as f32 / BREATH_MS as f32 * std::f32::consts::TAU).cos();
            (0.9 * b, 0.15 + 0.4 * b, 1.0, RING, GLOW)
        }
    }
}

/// Render one frame of the ring into a 32×32 premultiplied BGRA buffer.
/// Specks are not drawn here; see `render_specks`.
pub fn render(buf: &mut [u32], anim: &Anim) {
    debug_assert!(buf.len() >= SIZE * SIZE);
    buf.iter_mut().for_each(|p| *p = 0);

    let (grow, bright, halo, palette, glow) = if anim.in_transition() {
        // Mid-transition: blend the two looks so the ring eases between them.
        let (g0, b0, h0, p0, gl0) = look(anim.prev, anim.t_ms);
        let (g1, b1, h1, p1, gl1) = look(anim.mood, anim.t_ms);
        let k = anim.blend;
        let mut p = p1;
        for (i, c) in p.iter_mut().enumerate() {
            *c = p0[i].lerp(p1[i], k);
        }
        (lerp(g0, g1, k), lerp(b0, b1, k), lerp(h0, h1, k), p, gl0.lerp(gl1, k))
    } else {
        look(anim.mood, anim.t_ms)
    };
    let phase = anim.phase;
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
            } else if r < glow_out && halo > 0.0 {
                // Sparse specks in the halo; a few of them twinkle.
                let h = hash2(x as i32, y as i32, 7);
                let density = (h % 100) as f32 / 100.0;
                let falloff = 1.0 - (r - ring_out) / (glow_out - ring_out);
                // Twinkles re-roll about once a second rather than every frame.
                let twinkle = hash2(x as i32, y as i32, anim.t_ms / 1000) % 13 == 0;
                if density < 0.22 * falloff + bright * 0.15 || twinkle {
                    let a = if twinkle { 200 } else { (110.0 * falloff) as u8 + 40 };
                    let col = if twinkle { palette[4] } else { glow };
                    put(buf, x, y, col, (a as f32 * halo) as u8);
                }
            }
        }
    }
}

/// Draw one speck as a 4×4 block of sub-pixels (one sprite pixel wide) at a
/// continuous position; `weight` scales its alpha during a transition.
fn speck(over: &mut [u32], hx: f32, hy: f32, c: Rgb, a: u8, weight: f32) {
    let a = a as f32 * weight;
    if a < 0.5 {
        return;
    }
    // A one-sprite-pixel block (4×4 sub-pixels) at a continuous position: every
    // sub-pixel it touches gets the block's alpha scaled by how much of the cell it
    // covers, so a speck slides between cells instead of hopping. The overlay is
    // premultiplied, so `pixel()` with the scaled alpha is the whole blend.
    let (bx, by) = (hx * 4.0, hy * 4.0);
    let (x0, y0) = (bx.floor() as i32, by.floor() as i32);
    for y in y0..=y0 + 4 {
        let cy = ((y as f32 + 1.0).min(by + 4.0) - (y as f32).max(by)).max(0.0);
        if cy <= 0.0 || !(0..SPECK_SIZE as i32).contains(&y) {
            continue;
        }
        for x in x0..=x0 + 4 {
            let cx = ((x as f32 + 1.0).min(bx + 4.0) - (x as f32).max(bx)).max(0.0);
            if cx <= 0.0 || !(0..SPECK_SIZE as i32).contains(&x) {
                continue;
            }
            let aa = (a * cx * cy).round() as u8;
            if aa == 0 {
                continue;
            }
            let i = y as usize * SPECK_SIZE + x as usize;
            // Later specks win only where they are brighter; keeps overlaps crisp.
            if (over[i] >> 24) < aa as u32 {
                over[i] = pixel(c, aa);
            }
        }
    }
}

/// The specks of one mood at `weight` (1 = fully present).
fn specks_of(over: &mut [u32], mood: Mood, anim: &Anim, weight: f32) {
    let t_ms = anim.t_ms;
    let c = (SIZE as f32 - 1.0) / 2.0;
    let core_r = 6.4;
    let (grow, ..) = look(mood, t_ms);
    let ring_out = 9.6 + grow;
    let glow_out = 12.5 + grow;
    match mood {
        // A pixel or two spiralling into the core while something hovers.
        Mood::Hungry => {
            const FALL_MS: u32 = 1260;
            for k in 0..2u32 {
                let t = ((t_ms + k * FALL_MS / 2) % FALL_MS) as f32 / FALL_MS as f32;
                let e = t * t * (3.0 - 2.0 * t); // ease in and out
                let a = anim.phase * 2.0 + k as f32 * 3.1;
                let rr = ring_out + 2.5 - e * (ring_out + 2.5 - core_r + 1.0);
                speck(over, c + rr * a.cos(), c + rr * a.sin() * 0.8, RING[4], 255, weight);
            }
        }
        // While digesting, the halo specks stream into the core: each particle
        // spawns at a random angle and radius out in the halo, falls in on its
        // own schedule, and respawns somewhere else.
        Mood::Digesting => {
            const PARTICLES: u32 = 14;
            for k in 0..PARTICLES {
                let offset = hash2(k as i32, 0, 101) % 3600;
                let period = 1080 + hash2(k as i32, 1, 103) % 900; // 1.1..2 s to fall
                let life = t_ms + offset;
                let cycle = life / period;
                let t = (life % period) as f32 / period as f32; // 0 = spawn .. 1 = swallowed
                let h = hash2(k as i32, cycle as i32, 107);
                let a0 = (h % 628) as f32 / 100.0;
                let r0 = glow_out + 1.5 + ((h >> 10) % 4) as f32;
                // Ease in: a slow start, then it accelerates and curves as it nears the horizon.
                let e = t * t;
                let rr = r0 - e * (r0 - core_r + 0.5);
                let a = a0 + e * 1.6;
                let col = if t < 0.35 { GLOW } else { RING[4] };
                // Fade up over the first stretch so a spawn never pops.
                let alpha = if t < 0.15 { 80 + (t / 0.15 * 175.0) as u8 } else { 255 };
                speck(over, c + rr * a.cos(), c + rr * a.sin() * 0.8, col, alpha, weight);
            }
        }
        // One bright speck circling the halo once a second, with a short tail.
        Mood::Thinking => {
            let f = (t_ms % ORBIT_MS) as f32 / ORBIT_MS as f32;
            let rr = ring_out + 1.8;
            for (i, (col, a)) in [(RING[4], 255u8), (RING[3], 150), (GLOW, 90)].into_iter().enumerate() {
                let ang = (f - i as f32 * 0.035) * std::f32::consts::TAU;
                speck(over, c + rr * ang.cos(), c + rr * ang.sin() * 0.8, col, a, weight);
            }
        }
        _ => {}
    }
}

/// Render the moving specks into a 128×128 premultiplied BGRA overlay (four
/// sub-pixels per sprite pixel). Returns false when nothing was drawn, so the
/// caller can skip compositing.
pub fn render_specks(over: &mut [u32], anim: &Anim) -> bool {
    debug_assert!(over.len() >= SPECK_SIZE * SPECK_SIZE);
    over.iter_mut().for_each(|p| *p = 0);
    let mut drawn = false;
    if anim.in_transition() && has_specks(anim.prev) {
        specks_of(over, anim.prev, anim, 1.0 - anim.blend);
        drawn = true;
    }
    if has_specks(anim.mood) {
        specks_of(over, anim.mood, anim, if anim.in_transition() { anim.blend } else { 1.0 });
        drawn = true;
    }
    drawn
}
