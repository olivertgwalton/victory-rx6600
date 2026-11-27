//! X3DAudio as the console's `XAudio2_9.dll` computes it.
//!
//! The rest of XAudio2 passes through to the host's `xaudio2_9.dll`, but
//! the host's X3DAudio (FAudio) pans and filters differently: it splits a
//! source between two speakers 0.75/0.25 where the console gives
//! 0.707/0.293, ignores the emitter cone's LPF and reverb, and has no LFE
//! channel handling of the console's kind. These two exports answer for
//! `xaudio2_9.dll` with the console's arithmetic, in its order of
//! operations, so a title's 3D mix sounds as it does on the console.
//!
//! Where the console's own code faults (an LFE redirect or LFE gain on a
//! layout without an LFE speaker writes 16 GiB past the matrix, a curve
//! with no points reads before it) the write or read is skipped instead.
#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ptr;

use super::xgameruntime::{HRESULT, S_OK};
use crate::{HleModule, hle_export};

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Vector {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Cone {
    pub inner_angle: f32,
    pub outer_angle: f32,
    pub inner_volume: f32,
    pub outer_volume: f32,
    pub inner_lpf: f32,
    pub outer_lpf: f32,
    pub inner_reverb: f32,
    pub outer_reverb: f32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct CurvePoint {
    pub distance: f32,
    pub setting: f32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Curve {
    pub points: *const CurvePoint,
    pub count: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Listener {
    pub front: Vector,
    pub top: Vector,
    pub position: Vector,
    pub velocity: Vector,
    pub cone: *const Cone,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Emitter {
    pub cone: *const Cone,
    pub front: Vector,
    pub top: Vector,
    pub position: Vector,
    pub velocity: Vector,
    pub inner_radius: f32,
    pub inner_radius_angle: f32,
    pub channel_count: u32,
    pub channel_radius: f32,
    pub azimuths: *const f32,
    pub volume_curve: *const Curve,
    pub lfe_curve: *const Curve,
    pub lpf_direct_curve: *const Curve,
    pub lpf_reverb_curve: *const Curve,
    pub reverb_curve: *const Curve,
    pub curve_distance_scaler: f32,
    pub doppler_scaler: f32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct DspSettings {
    /// `[dst * src_channels + src]`.
    pub matrix: *mut f32,
    pub delay: *mut f32,
    pub src_channels: u32,
    pub dst_channels: u32,
    pub lpf_direct: f32,
    pub lpf_reverb: f32,
    pub reverb: f32,
    pub doppler: f32,
    pub angle: f32,
    pub distance: f32,
    pub emitter_velocity: f32,
    pub listener_velocity: f32,
}

/// The 20-byte `X3DAUDIO_HANDLE` as the console fills it.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
struct Handle {
    mask: u32,
    channels: u32,
    /// The LFE speaker's index, `NO_LFE` without one.
    lfe: u32,
    speed: f32,
    /// The float just below `speed`: velocities are clamped to it.
    speed_below: f32,
}

const _: () = {
    use std::mem::{offset_of, size_of};
    assert!(size_of::<Handle>() == 20);
    assert!(offset_of!(Listener, cone) == 0x30);
    assert!(offset_of!(Emitter, front) == 0x8);
    assert!(offset_of!(Emitter, inner_radius) == 0x38);
    assert!(offset_of!(Emitter, channel_count) == 0x40);
    assert!(offset_of!(Emitter, azimuths) == 0x48);
    assert!(offset_of!(Emitter, reverb_curve) == 0x70);
    assert!(offset_of!(Emitter, doppler_scaler) == 0x7c);
    assert!(offset_of!(DspSettings, lpf_direct) == 0x18);
    assert!(offset_of!(DspSettings, listener_velocity) == 0x34);
};

const NO_LFE: u32 = u32::MAX;

const MATRIX: u32 = 0x1;
const DELAY: u32 = 0x2;
const LPF_DIRECT: u32 = 0x4;
const LPF_REVERB: u32 = 0x8;
const REVERB: u32 = 0x10;
const DOPPLER: u32 = 0x20;
const EMITTER_ANGLE: u32 = 0x40;
const ZEROCENTER: u32 = 0x1_0000;
const REDIRECT_TO_LFE: u32 = 0x2_0000;

/// `FLT_EPSILON`, the console's "zero" for distances.
const EPSILON: f32 = f32::from_bits(0x3400_0000);
/// Below this inner radius (halved) no source is spread.
const MIN_SPREAD: f32 = f32::from_bits(0x34c0_0000);
const PI: f32 = f32::from_bits(0x4049_0fdb);
const TWO_PI: f32 = f32::from_bits(0x40c9_0fdb);
const HALF_PI: f32 = f32::from_bits(0x3fc9_0fdb);
const QUARTER_PI: f32 = f32::from_bits(0x3f49_0fdb);
const EIGHTH_PI: f32 = f32::from_bits(0x3ec9_0fdb);
const THREE_QUARTER_PI: f32 = f32::from_bits(0x4016_cbe4);

// Speaker azimuths the panner walks, per layout (center left out with
// ZEROCENTER, LFE always left out).
const FRONT: [f32; 2] = [-HALF_PI, HALF_PI];
const QUAD: [f32; 4] = [-QUARTER_PI, QUARTER_PI, -THREE_QUARTER_PI, THREE_QUARTER_PI];
const FIVE_ONE: [f32; 5] = [
    -QUARTER_PI,
    QUARTER_PI,
    0.0,
    -THREE_QUARTER_PI,
    THREE_QUARTER_PI,
];
const SURROUND: [f32; 4] = [-QUARTER_PI, QUARTER_PI, 0.0, PI];
const SURROUND_ZC: [f32; 3] = [-QUARTER_PI, QUARTER_PI, PI];
const FIVE_ONE_SIDE: [f32; 5] = [-QUARTER_PI, QUARTER_PI, 0.0, -HALF_PI, HALF_PI];
const FIVE_ONE_SIDE_ZC: [f32; 4] = [-QUARTER_PI, QUARTER_PI, -HALF_PI, HALF_PI];
const SEVEN_ONE: [f32; 7] = [
    -QUARTER_PI,
    QUARTER_PI,
    0.0,
    -THREE_QUARTER_PI,
    THREE_QUARTER_PI,
    -EIGHTH_PI,
    EIGHTH_PI,
];
const SEVEN_ONE_ZC: [f32; 6] = [
    -QUARTER_PI,
    QUARTER_PI,
    -THREE_QUARTER_PI,
    THREE_QUARTER_PI,
    -EIGHTH_PI,
    EIGHTH_PI,
];
const SEVEN_ONE_SIDE: [f32; 7] = [
    -QUARTER_PI,
    QUARTER_PI,
    0.0,
    -THREE_QUARTER_PI,
    THREE_QUARTER_PI,
    -HALF_PI,
    HALF_PI,
];
const SEVEN_ONE_SIDE_ZC: [f32; 6] = [
    -QUARTER_PI,
    QUARTER_PI,
    -THREE_QUARTER_PI,
    THREE_QUARTER_PI,
    -HALF_PI,
    HALF_PI,
];

// Default curves when the emitter gives none.
const LPF_DIRECT_DEFAULT: [CurvePoint; 2] = [
    CurvePoint {
        distance: 0.0,
        setting: 1.0,
    },
    CurvePoint {
        distance: 1.0,
        setting: 0.75,
    },
];
const LPF_REVERB_DEFAULT: [CurvePoint; 2] = [
    CurvePoint {
        distance: 0.0,
        setting: 0.75,
    },
    CurvePoint {
        distance: 1.0,
        setting: 0.75,
    },
];
const REVERB_DEFAULT: [CurvePoint; 2] = [
    CurvePoint {
        distance: 0.0,
        setting: 1.0,
    },
    CurvePoint {
        distance: 1.0,
        setting: 0.0,
    },
];

/// Stores the layout and speed of sound; the console checks neither.
pub extern "system" fn X3DAudioInitialize(mask: u32, speed: f32, handle: *mut u8) -> HRESULT {
    let lfe = if mask & 0x8 != 0 {
        (mask & 1) + (mask >> 1 & 1) + (mask >> 2 & 1)
    } else {
        NO_LFE
    };
    let h = Handle {
        mask,
        channels: mask.count_ones().max(1),
        lfe,
        speed,
        speed_below: f32::from_bits(speed.to_bits().wrapping_sub(1)),
    };
    // SAFETY: the title's 20-byte handle, of any alignment.
    unsafe { ptr::write_unaligned(handle.cast::<Handle>(), h) };
    S_OK
}

pub extern "system" fn X3DAudioCalculate(
    handle: *const u8,
    listener: *const Listener,
    emitter: *const Emitter,
    flags: u32,
    dsp: *mut DspSettings,
) {
    // SAFETY: the title's handle from `X3DAudioInitialize` and its own
    // structures, valid for the call as X3DAudio requires.
    unsafe {
        let h = ptr::read_unaligned(handle.cast::<Handle>());
        calculate(&h, &*listener, &*emitter, flags, &mut *dsp);
    }
}

fn sub(a: Vector, b: Vector) -> Vector {
    Vector {
        x: a.x - b.x,
        y: a.y - b.y,
        z: a.z - b.z,
    }
}

/// `(a.y*b.y + a.x*b.x) + a.z*b.z`, the console's order.
fn dot(a: Vector, b: Vector) -> f32 {
    (a.y * b.y + a.x * b.x) + a.z * b.z
}

/// SSE `maxss a, b`.
fn maxss(a: f32, b: f32) -> f32 {
    if a > b { a } else { b }
}

/// Matrix entry `i`.
///
/// # Safety
/// `i` is inside the title's matrix.
unsafe fn m(s: &DspSettings, i: usize) -> *mut f32 {
    unsafe { s.matrix.add(i) }
}

unsafe fn azimuth(e: &Emitter, i: usize) -> f32 {
    unsafe { *e.azimuths.add(i) }
}

unsafe fn calculate(h: &Handle, l: &Listener, e: &Emitter, flags: u32, s: &mut DspSettings) {
    let d = sub(l.position, e.position);
    let (dx, dy, dz) = (d.x as f64, d.y as f64, d.z as f64);
    s.distance = ((dy * dy + dx * dx) + dz * dz).sqrt() as f32;
    if flags & EMITTER_ANGLE != 0 {
        let cos = if s.distance > EPSILON {
            dot(d, e.front) / s.distance
        } else {
            0.0
        };
        s.angle = crt::acosf(cos);
    }
    unsafe {
        if flags & (MATRIX | LPF_DIRECT | LPF_REVERB | REVERB) != 0 {
            curves(h, e, flags, s);
            if e.channel_count == 1 && !e.cone.is_null() {
                emitter_cone(h, l, e, flags, s);
            }
            if !l.cone.is_null() {
                listener_cone(h, l, e, flags, s);
            }
            if flags & MATRIX != 0 {
                matrix(h, l, e, flags, s);
            }
        }
        if flags & DELAY != 0 {
            delay(l, e, s);
        }
    }
    if flags & DOPPLER != 0 {
        doppler(h, l, e, s);
    }
}

/// Piecewise linear; past the last point its setting.
unsafe fn eval(curve: *const Curve, x: f32) -> f32 {
    let c = unsafe { &*curve };
    let n = c.count as usize;
    if n == 0 {
        // The console reads point 0xffffffff here.
        return 0.0;
    }
    // SAFETY: the title's points.
    let p = unsafe { std::slice::from_raw_parts(c.points, n) };
    let mut i = 1;
    while i < n && !(p[i].distance > x) {
        i += 1;
    }
    if i >= n {
        return p[n - 1].setting;
    }
    (p[i].setting - p[i - 1].setting) / (p[i].distance - p[i - 1].distance)
        * (x - p[i - 1].distance)
        + p[i - 1].setting
}

unsafe fn eval_or(curve: *const Curve, default: &[CurvePoint; 2], x: f32) -> f32 {
    let fallback = Curve {
        points: default.as_ptr(),
        count: 2,
    };
    unsafe { eval(if curve.is_null() { &fallback } else { curve }, x) }
}

/// The volume and LFE curves without one: inverse distance past 1.
unsafe fn attenuation(curve: *const Curve, x: f32) -> f32 {
    if curve.is_null() {
        if x > 1.0 { 1.0 / x } else { 1.0 }
    } else {
        unsafe { eval(curve, x) }
    }
}

/// Curve settings, and per source channel its gain in the matrix's first
/// row (the LFE channels' in the LFE row) for `matrix` to pan.
unsafe fn curves(h: &Handle, e: &Emitter, flags: u32, s: &mut DspSettings) {
    let x = s.distance / e.curve_distance_scaler;
    unsafe {
        if flags & LPF_DIRECT != 0 {
            s.lpf_direct = eval_or(e.lpf_direct_curve, &LPF_DIRECT_DEFAULT, x);
        }
        if flags & LPF_REVERB != 0 {
            s.lpf_reverb = eval_or(e.lpf_reverb_curve, &LPF_REVERB_DEFAULT, x);
        }
        if flags & REVERB != 0 {
            s.reverb = eval_or(e.reverb_curve, &REVERB_DEFAULT, x);
        }
        if flags & MATRIX == 0 {
            return;
        }
        let gain = attenuation(e.volume_curve, x);
        let src = s.src_channels as usize;
        let dst = s.dst_channels as usize;
        if e.channel_count == 1 {
            *m(s, 0) = gain;
            for d in 1..dst {
                *m(s, d * src) = 0.0;
            }
            if flags & REDIRECT_TO_LFE != 0 {
                let lfe = attenuation(e.lfe_curve, x);
                if h.lfe != NO_LFE {
                    *m(s, h.lfe as usize) = lfe;
                }
            }
            return;
        }
        let count = e.channel_count as usize;
        let zero_column = |i: usize| {
            for d in 0..dst {
                *m(s, d * src + i) = 0.0;
            }
        };
        let mut i = 0;
        while i < count {
            zero_column(i);
            if azimuth(e, i) == TWO_PI {
                if h.lfe != NO_LFE {
                    break;
                }
            } else {
                *m(s, i) = gain;
            }
            i += 1;
        }
        if i < count {
            // An LFE channel and an LFE speaker: the LFE curve goes to it,
            // and nothing is redirected.
            let lfe = attenuation(e.lfe_curve, x);
            let row = s.src_channels.wrapping_mul(h.lfe) as usize;
            *m(s, row + i) = lfe;
            for i in i + 1..count {
                zero_column(i);
                if azimuth(e, i) == TWO_PI {
                    *m(s, row + i) = lfe;
                } else {
                    *m(s, i) = gain;
                }
            }
        } else if flags & REDIRECT_TO_LFE != 0 {
            let lfe = attenuation(e.lfe_curve, x) / s.src_channels as f32;
            if h.lfe != NO_LFE {
                let row = s.src_channels.wrapping_mul(h.lfe) as usize;
                for k in 0..src {
                    *m(s, row + k) = lfe;
                }
            }
        }
    }
}

/// Volume, LPF and reverb factors of a cone for a source whose direction
/// has cosine `cos` with the cone's axis.
fn cone_factors(cone: &Cone, distance: f32, cos: impl FnOnce() -> f32) -> (f32, f32, f32) {
    let inner = (cone.inner_volume, cone.inner_lpf, cone.inner_reverb);
    let outer = (cone.outer_volume, cone.outer_lpf, cone.outer_reverb);
    if cone.outer_angle == 0.0 {
        return outer;
    }
    if cone.inner_angle == TWO_PI || EPSILON >= distance {
        return inner;
    }
    let c = cos();
    let ci = crt::cosf(cone.inner_angle * 0.5);
    if c >= ci {
        return inner;
    }
    let co = crt::cosf(cone.outer_angle * 0.5);
    if co >= c {
        return outer;
    }
    let t = (c - ci) / (co - ci);
    (
        (cone.outer_volume - cone.inner_volume) * t + cone.inner_volume,
        (cone.outer_lpf - cone.inner_lpf) * t + cone.inner_lpf,
        (cone.outer_reverb - cone.inner_reverb) * t + cone.inner_reverb,
    )
}

unsafe fn emitter_cone(h: &Handle, l: &Listener, e: &Emitter, flags: u32, s: &mut DspSettings) {
    let distance = s.distance;
    let (volume, lpf, reverb) = cone_factors(unsafe { &*e.cone }, distance, || {
        dot(sub(l.position, e.position), e.front) / distance
    });
    unsafe {
        if flags & MATRIX != 0 {
            *m(s, 0) *= volume;
            if h.lfe != NO_LFE {
                *m(s, h.lfe as usize) *= volume;
            }
        }
    }
    if flags & LPF_DIRECT != 0 {
        s.lpf_direct *= lpf;
    }
    if flags & LPF_REVERB != 0 {
        s.lpf_reverb *= lpf;
    }
    if flags & REVERB != 0 {
        s.reverb *= reverb;
    }
}

unsafe fn listener_cone(h: &Handle, l: &Listener, e: &Emitter, flags: u32, s: &mut DspSettings) {
    let distance = s.distance;
    let (volume, lpf, reverb) = cone_factors(unsafe { &*l.cone }, distance, || {
        dot(sub(e.position, l.position), l.front) / distance
    });
    unsafe {
        if flags & MATRIX != 0 {
            let count = e.channel_count as usize;
            for i in 0..count {
                *m(s, i) *= volume;
            }
            if h.lfe != NO_LFE {
                if count == 1 {
                    *m(s, h.lfe as usize) *= volume;
                } else {
                    let row = s.src_channels.wrapping_mul(h.lfe) as usize;
                    for i in 0..count {
                        if azimuth(e, i) == TWO_PI {
                            *m(s, row + i) *= volume;
                        }
                    }
                }
            }
        }
    }
    if flags & LPF_DIRECT != 0 {
        s.lpf_direct -= lpf;
        if 0.0 > s.lpf_direct {
            s.lpf_direct = 0.0;
        }
    }
    if flags & LPF_REVERB != 0 {
        s.lpf_reverb -= lpf;
        if 0.0 > s.lpf_reverb {
            s.lpf_reverb = 0.0;
        }
    }
    if flags & REVERB != 0 {
        s.reverb *= reverb;
    }
}

/// How many speakers a source at the listener spreads over.
fn spread_count(h: &Handle, flags: u32, dst: u32) -> f32 {
    let n = if h.lfe == NO_LFE {
        dst
    } else {
        dst.wrapping_sub(1)
    };
    let n = if flags & ZEROCENTER == 0 {
        n
    } else {
        n.wrapping_sub(1)
    };
    n as f32
}

/// The speakers a spread source reaches: all but the LFE and, with
/// ZEROCENTER, the third (center).
fn spread_to(h: &Handle, flags: u32, d: usize) -> bool {
    d as u64 != h.lfe as u64 && !(d == 2 && flags & ZEROCENTER != 0)
}

unsafe fn matrix(h: &Handle, l: &Listener, e: &Emitter, flags: u32, s: &mut DspSettings) {
    let f = l.front;
    let t = l.top;
    // The listener's right (top x front) and up (front x right).
    let r = Vector {
        x: t.y * f.z - t.z * f.y,
        y: t.z * f.x - t.x * f.z,
        z: t.x * f.y - t.y * f.x,
    };
    let u = Vector {
        x: r.z * f.y - r.y * f.z,
        y: r.x * f.z - r.z * f.x,
        z: r.y * f.x - r.x * f.y,
    };
    let count = e.channel_count as usize;
    if count == 0 {
        return;
    }
    let radius = e.channel_radius;
    let (rfx, rfy, rfz) = if count > 1 {
        (radius * e.front.x, radius * e.front.y, radius * e.front.z)
    } else {
        (0.0, 0.0, 0.0)
    };
    let src = s.src_channels as usize;
    let dst = s.dst_channels as usize;
    for i in 0..count {
        let p = if count > 1 {
            let az = unsafe { azimuth(e, i) };
            if az == TWO_PI {
                // LFE channels were placed by `curves`.
                continue;
            }
            // The emitter's front, scaled by the channel radius, turned by
            // the azimuth around the emitter's top.
            let c = crt::cosf(az);
            let sn = crt::sinf(az);
            let k = e.top;
            let t = 1.0 - c;
            let kxt = k.x * t;
            let kyt = k.y * t;
            Vector {
                x: (((kxt * k.x + c) * rfx + (k.y * kxt - k.z * sn) * rfy)
                    + (k.y * sn + k.z * kxt) * rfz)
                    + e.position.x,
                y: (((kyt * k.y + c) * rfy + (k.z * sn + k.y * kxt) * rfx)
                    + (kyt * k.z - k.x * sn) * rfz)
                    + e.position.y,
                z: (((k.x * sn + kyt * k.z) * rfy + (k.z * kxt - k.y * sn) * rfx)
                    + ((k.z * t) * k.z + c) * rfz)
                    + e.position.z,
            }
        } else {
            e.position
        };
        let v = sub(p, l.position);
        let x = dot(v, r);
        let z = dot(v, f);
        let y = dot(v, u);
        let horizontal = (z * z + x * x).sqrt();
        unsafe {
            let gain = *m(s, i);
            *m(s, i) = 0.0;
            if EPSILON >= horizontal {
                // At the listener: every speaker alike.
                let share = gain / spread_count(h, flags, s.dst_channels);
                for d in 0..dst {
                    if spread_to(h, flags, d) {
                        *m(s, d * src + i) = share;
                    }
                }
                continue;
            }
            let angle = crt::atan2f(x, z);
            let inner = crt::sinf(e.inner_radius_angle) * y + e.inner_radius;
            if horizontal >= inner {
                pan(h, gain, angle, flags, s, i);
            } else {
                // Inside the inner radius part of it comes from behind.
                let back = crt::tanf((inner - horizontal) * HALF_PI / (inner + inner));
                let front = gain / (back + 1.0);
                pan(h, front, angle, flags, s, i);
                pan(h, front * back, angle + PI, flags, s, i);
            }
            let near = maxss(MIN_SPREAD, e.inner_radius * 0.5);
            if near >= horizontal {
                // Close in, blended towards every speaker alike.
                let k = (horizontal - EPSILON) / near;
                let share = gain / spread_count(h, flags, s.dst_channels);
                for d in 0..dst {
                    if spread_to(h, flags, d) {
                        let entry = m(s, d * src + i);
                        *entry *= k;
                        *entry += (1.0 - k) * share;
                    }
                }
            }
        }
    }
}

/// Adds `gain` from `angle` (0 ahead, positive to the right) to the two
/// speakers either side of it, `1/(1+t)` and `t/(1+t)` with
/// `t = tan(pi/2 * a/(a+b))` for the angles `a`, `b` to them.
unsafe fn pan(h: &Handle, gain: f32, angle: f32, flags: u32, s: &mut DspSettings, i: usize) {
    let zc = flags & ZEROCENTER != 0;
    let table: &[f32] = match h.mask {
        0x3 | 0xb => &FRONT,
        0x33 | 0x3b => &QUAD,
        0x3f if zc => &QUAD,
        0x3f => &FIVE_ONE,
        0x107 if zc => &SURROUND_ZC,
        0x107 => &SURROUND,
        0x60f if zc => &FIVE_ONE_SIDE_ZC,
        0x60f => &FIVE_ONE_SIDE,
        0xff if zc => &SEVEN_ONE_ZC,
        0xff => &SEVEN_ONE,
        0x63f if zc => &SEVEN_ONE_SIDE_ZC,
        0x63f => &SEVEN_ONE_SIDE,
        0x4 => {
            unsafe { *m(s, i) += gain };
            return;
        }
        // Other layouts have no table: everything to the first speaker.
        _ => &[],
    };
    let mut before = -4.0f32;
    let mut after = 4.0f32;
    let mut a = None;
    let mut b = None;
    for (k, &speaker) in table.iter().enumerate() {
        let mut d = angle - speaker;
        if d > PI {
            d += -TWO_PI;
        } else if -PI > d {
            d += TWO_PI;
        }
        if 0.0 >= d && d > before {
            a = Some(k);
            before = d;
            continue;
        }
        if d >= 0.0 && after > d {
            b = Some(k);
            after = d;
        }
    }
    if 0.0 > before {
        before = -before;
    }
    if 0.0 > after {
        after = -after;
    }
    let (mut a, mut b) = match (a, b) {
        (Some(a), Some(b)) => (a, b),
        _ => (0, 0),
    };
    // Table indices to speaker indices: past the center, then the LFE.
    if zc {
        if a > 1 {
            a += 1;
        }
        if b > 1 {
            b += 1;
        }
    }
    if h.lfe != NO_LFE {
        let past = if h.mask & 0x4 != 0 { 2 } else { 1 };
        if a > past {
            a += 1;
        }
        if b > past {
            b += 1;
        }
    }
    let src = s.src_channels as usize;
    unsafe {
        if EPSILON > before {
            *m(s, a * src + i) += gain;
            return;
        }
        if EPSILON > after {
            *m(s, b * src + i) += gain;
            return;
        }
        let t = crt::tanf(before * HALF_PI / (after + before));
        let near = gain / (t + 1.0);
        *m(s, a * src + i) += near;
        *m(s, b * src + i) += t * near;
    }
}

/// The interaural delay, in the first entry for sources to the right and
/// the second for the left.
unsafe fn delay(l: &Listener, e: &Emitter, s: &mut DspSettings) {
    let (f, t) = (l.front, l.top);
    let r = Vector {
        x: f.z * t.y - f.y * t.z,
        y: f.x * t.z - t.x * f.z,
        z: t.x * f.y - f.x * t.y,
    };
    let v = sub(e.position, l.position);
    let x = dot(v, r);
    let z = dot(v, f);
    unsafe {
        let out = std::slice::from_raw_parts_mut(s.delay, 2);
        if EPSILON >= x && EPSILON >= z && x >= -EPSILON && z >= -EPSILON {
            out[0] = 0.0;
            out[1] = 0.0;
            return;
        }
        let a = crt::atan2(x as f64, z as f64);
        if x > 0.0 {
            out[0] = crt::sin(a) as f32;
            out[1] = 0.0;
        } else {
            out[0] = 0.0;
            out[1] = crt::sin(-a) as f32;
        }
    }
}

fn doppler(h: &Handle, l: &Listener, e: &Emitter, s: &mut DspSettings) {
    let mut factor = 1.0;
    let mut listener = 0.0;
    let mut emitter = 0.0;
    if s.distance > EPSILON {
        let d = sub(l.position, e.position);
        listener = dot(d, l.velocity) / s.distance;
        emitter = dot(d, e.velocity) / s.distance;
        let limit = h.speed_below;
        let clamp = |v: f32| if v > limit { limit } else { maxss(-limit, v) };
        let sl = clamp(e.doppler_scaler * listener);
        let se = clamp(e.doppler_scaler * emitter);
        factor = (h.speed - sl) / (h.speed - se);
    }
    s.doppler = factor;
    s.emitter_velocity = emitter;
    s.listener_velocity = listener;
}

/// The C runtime's math, which the console DLL imports (`_o_atan2f` and
/// the like): the host's `ucrtbase.dll` where there is one, so results
/// round as the console's do. A source exactly between two speakers is
/// panned by which way `atan2f` rounds, and mingw's libm rounds otherwise.
mod crt {
    #[cfg(windows)]
    struct Crt {
        acosf: unsafe extern "C" fn(f32) -> f32,
        cosf: unsafe extern "C" fn(f32) -> f32,
        sinf: unsafe extern "C" fn(f32) -> f32,
        tanf: unsafe extern "C" fn(f32) -> f32,
        atan2f: unsafe extern "C" fn(f32, f32) -> f32,
        sin: unsafe extern "C" fn(f64) -> f64,
        atan2: unsafe extern "C" fn(f64, f64) -> f64,
    }

    #[cfg(windows)]
    fn crt() -> Option<&'static Crt> {
        static CRT: std::sync::OnceLock<Option<Crt>> = std::sync::OnceLock::new();
        CRT.get_or_init(|| {
            let f = |name| crate::host::function("ucrtbase.dll", name);
            // SAFETY: the CRT's own functions, with their C signatures.
            unsafe {
                Some(Crt {
                    acosf: std::mem::transmute::<usize, _>(f("acosf")?),
                    cosf: std::mem::transmute::<usize, _>(f("cosf")?),
                    sinf: std::mem::transmute::<usize, _>(f("sinf")?),
                    tanf: std::mem::transmute::<usize, _>(f("tanf")?),
                    atan2f: std::mem::transmute::<usize, _>(f("atan2f")?),
                    sin: std::mem::transmute::<usize, _>(f("sin")?),
                    atan2: std::mem::transmute::<usize, _>(f("atan2")?),
                })
            }
        })
        .as_ref()
    }

    macro_rules! math {
        ($name:ident($($arg:ident: $t:ty),+) -> $r:ty, $fallback:expr) => {
            pub(super) fn $name($($arg: $t),+) -> $r {
                #[cfg(windows)]
                if let Some(c) = crt() {
                    // SAFETY: a pure math function.
                    return unsafe { (c.$name)($($arg),+) };
                }
                $fallback
            }
        };
    }

    math!(acosf(x: f32) -> f32, x.acos());
    math!(cosf(x: f32) -> f32, x.cos());
    math!(sinf(x: f32) -> f32, x.sin());
    math!(tanf(x: f32) -> f32, x.tan());
    math!(atan2f(y: f32, x: f32) -> f32, y.atan2(x));
    math!(sin(x: f64) -> f64, x.sin());
    math!(atan2(y: f64, x: f64) -> f64, y.atan2(x));
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "x3daudio",
        // Only these two: XAudio2 itself stays the host's.
        dlls: &["xaudio2_9.dll"],
        exports: vec![
            hle_export!(X3DAudioInitialize),
            hle_export!(X3DAudioCalculate),
        ],
    }
}

#[cfg(test)]
mod tests;
