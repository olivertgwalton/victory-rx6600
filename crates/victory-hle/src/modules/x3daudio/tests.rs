//! Checked against the console's `XAudio2_9.dll`: `vectors.txt` holds its
//! inputs and outputs for generated cases (a harness outside the repo calls
//! its X3DAudio under Wine and prints the float bits). Every output starts
//! as 0.125 there and here, so what is written is compared as well as what.
//! `VICTORY_X3D_VECTORS=<file>` checks another such file too.

use super::*;

const SENTINEL: f32 = 0.125;

struct Case {
    line: usize,
    mask: u32,
    speed: f32,
    flags: u32,
    listener: Listener,
    listener_cone: Option<Cone>,
    emitter: Emitter,
    emitter_cone: Option<Cone>,
    azimuths: Vec<f32>,
    curves: [Option<Vec<CurvePoint>>; 5],
    src: u32,
    dst: u32,
    /// Matrix, delays, then the eight scalar outputs.
    expected: Vec<f32>,
}

struct Tokens<'a>(std::str::SplitWhitespace<'a>);

impl Tokens<'_> {
    fn hex(&mut self) -> u32 {
        let mut t = self.0.next().expect("short line");
        if t == "|" {
            t = self.0.next().expect("short line");
        }
        u32::from_str_radix(t, 16).expect("hex")
    }
    fn f(&mut self) -> f32 {
        f32::from_bits(self.hex())
    }
    fn v(&mut self) -> Vector {
        Vector {
            x: self.f(),
            y: self.f(),
            z: self.f(),
        }
    }
    fn cone(&mut self) -> Option<Cone> {
        (self.hex() != 0).then(|| Cone {
            inner_angle: self.f(),
            outer_angle: self.f(),
            inner_volume: self.f(),
            outer_volume: self.f(),
            inner_lpf: self.f(),
            outer_lpf: self.f(),
            inner_reverb: self.f(),
            outer_reverb: self.f(),
        })
    }
    fn curve(&mut self) -> Option<Vec<CurvePoint>> {
        let n = self.hex();
        (n != 0).then(|| {
            (0..n)
                .map(|_| CurvePoint {
                    distance: self.f(),
                    setting: self.f(),
                })
                .collect()
        })
    }
}

fn parse(text: &str) -> Vec<Case> {
    let mut cases = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let Some(rest) = line.strip_prefix("V ") else {
            continue;
        };
        let mut t = Tokens(rest.split_whitespace());
        let (mask, speed, flags) = (t.hex(), t.f(), t.hex());
        let listener = Listener {
            front: t.v(),
            top: t.v(),
            position: t.v(),
            velocity: t.v(),
            cone: ptr::null(),
        };
        let listener_cone = t.cone();
        let emitter_cone = t.cone();
        let mut emitter = Emitter {
            cone: ptr::null(),
            front: t.v(),
            top: t.v(),
            position: t.v(),
            velocity: t.v(),
            inner_radius: t.f(),
            inner_radius_angle: t.f(),
            channel_count: t.hex(),
            channel_radius: t.f(),
            azimuths: ptr::null(),
            volume_curve: ptr::null(),
            lfe_curve: ptr::null(),
            lpf_direct_curve: ptr::null(),
            lpf_reverb_curve: ptr::null(),
            reverb_curve: ptr::null(),
            curve_distance_scaler: 0.0,
            doppler_scaler: 0.0,
        };
        let azimuths = (0..emitter.channel_count).map(|_| t.f()).collect();
        let curves = [t.curve(), t.curve(), t.curve(), t.curve(), t.curve()];
        emitter.curve_distance_scaler = t.f();
        emitter.doppler_scaler = t.f();
        let (src, dst) = (t.hex(), t.hex());
        let outputs = (src * dst + dst.max(2) + 8) as usize;
        let expected = (0..outputs).map(|_| t.f()).collect();
        assert!(t.0.next().is_none(), "line {}: trailing tokens", n + 1);
        cases.push(Case {
            line: n + 1,
            mask,
            speed,
            flags,
            listener,
            listener_cone,
            emitter,
            emitter_cone,
            azimuths,
            curves,
            src,
            dst,
            expected,
        });
    }
    cases
}

/// Ours on `case`, laid out like `Case::expected`.
fn run(case: &Case) -> Vec<f32> {
    run_with(case, 1).0
}

/// Ours on `case`, `rounds` times; the outputs and the time the calls took.
fn run_with(case: &Case, rounds: u32) -> (Vec<f32>, std::time::Duration) {
    let mut handle = [0xcdu8; 20];
    assert_eq!(
        X3DAudioInitialize(case.mask, case.speed, handle.as_mut_ptr()),
        S_OK
    );
    let curves: Vec<Option<Curve>> = case
        .curves
        .iter()
        .map(|c| {
            c.as_ref().map(|p| Curve {
                points: p.as_ptr(),
                count: p.len() as u32,
            })
        })
        .collect();
    let curve = |i: usize| {
        curves[i]
            .as_ref()
            .map_or(ptr::null(), |c| c as *const Curve)
    };
    let mut listener = case.listener;
    listener.cone = case.listener_cone.as_ref().map_or(ptr::null(), |c| c);
    let mut emitter = case.emitter;
    emitter.cone = case.emitter_cone.as_ref().map_or(ptr::null(), |c| c);
    emitter.azimuths = case.azimuths.as_ptr();
    emitter.volume_curve = curve(0);
    emitter.lfe_curve = curve(1);
    emitter.lpf_direct_curve = curve(2);
    emitter.lpf_reverb_curve = curve(3);
    emitter.reverb_curve = curve(4);
    let mut matrix = [SENTINEL; 64];
    let mut delay = [SENTINEL; 8];
    let mut s = DspSettings {
        matrix: matrix.as_mut_ptr(),
        delay: delay.as_mut_ptr(),
        src_channels: case.src,
        dst_channels: case.dst,
        lpf_direct: SENTINEL,
        lpf_reverb: SENTINEL,
        reverb: SENTINEL,
        doppler: SENTINEL,
        angle: SENTINEL,
        distance: SENTINEL,
        emitter_velocity: SENTINEL,
        listener_velocity: SENTINEL,
    };
    let start = std::time::Instant::now();
    for _ in 0..rounds {
        X3DAudioCalculate(handle.as_ptr(), &listener, &emitter, case.flags, &mut s);
    }
    let took = start.elapsed();
    let mut out: Vec<f32> = matrix[..(case.src * case.dst) as usize].to_vec();
    out.extend_from_slice(&delay[..case.dst.max(2) as usize]);
    out.extend_from_slice(&[
        s.lpf_direct,
        s.lpf_reverb,
        s.reverb,
        s.doppler,
        s.angle,
        s.distance,
        s.emitter_velocity,
        s.listener_velocity,
    ]);
    (out, took)
}

/// The harness's measure: relative past 1, absolute below.
fn differs(a: f32, b: f32) -> bool {
    if a.is_nan() || b.is_nan() {
        return a.is_nan() != b.is_nan();
    }
    if a == b {
        return false;
    }
    (a - b).abs() / 1f32.max(a.abs()).max(b.abs()) > 1e-4
}

fn check(cases: &[Case]) {
    let mut bad = Vec::new();
    for case in cases {
        let ours = run(case);
        let wrong: Vec<usize> = (0..ours.len())
            .filter(|&k| differs(ours[k], case.expected[k]))
            .collect();
        if !wrong.is_empty() {
            bad.push(format!(
                "line {} mask {:#x} flags {:#x} src {} dst {}: at {:?}\n  console {:?}\n  ours    {:?}",
                case.line, case.mask, case.flags, case.src, case.dst, wrong, case.expected, ours
            ));
        }
    }
    let shown: Vec<_> = bad.iter().take(5).cloned().collect();
    assert!(
        bad.is_empty(),
        "{} of {} cases differ from the console:\n{}",
        bad.len(),
        cases.len(),
        shown.join("\n")
    );
}

#[test]
fn matches_the_console_on_its_vectors() {
    let cases = parse(include_str!("vectors.txt"));
    assert!(cases.len() >= 500);
    check(&cases);
    if let Some(path) = std::env::var_os("VICTORY_X3D_VECTORS") {
        let cases = parse(&std::fs::read_to_string(path).unwrap());
        check(&cases);
        eprintln!("{} more cases match", cases.len());
    }
}

/// What the harness first showed: the console pans a source between two
/// speakers by `tan`, 0.707/0.293 at 45 degrees off one of them.
#[test]
fn stereo_pans_by_tangent() {
    let mut handle = [0u8; 20];
    X3DAudioInitialize(0x3, 343.5, handle.as_mut_ptr());
    let listener = Listener {
        front: Vector {
            x: 0.0,
            y: 0.0,
            z: 1.0,
        },
        top: Vector {
            x: 0.0,
            y: 1.0,
            z: 0.0,
        },
        position: Vector::default(),
        velocity: Vector::default(),
        cone: ptr::null(),
    };
    let emitter = Emitter {
        cone: ptr::null(),
        front: Vector {
            x: 0.0,
            y: 0.0,
            z: -1.0,
        },
        top: Vector {
            x: 0.0,
            y: 1.0,
            z: 0.0,
        },
        position: Vector {
            x: -1.0,
            y: 0.0,
            z: 1.0,
        },
        velocity: Vector::default(),
        inner_radius: 0.0,
        inner_radius_angle: 0.0,
        channel_count: 1,
        channel_radius: 0.0,
        azimuths: ptr::null(),
        volume_curve: ptr::null(),
        lfe_curve: ptr::null(),
        lpf_direct_curve: ptr::null(),
        lpf_reverb_curve: ptr::null(),
        reverb_curve: ptr::null(),
        curve_distance_scaler: 2.0,
        doppler_scaler: 1.0,
    };
    let mut matrix = [0f32; 2];
    let mut s = DspSettings {
        matrix: matrix.as_mut_ptr(),
        delay: ptr::null_mut(),
        src_channels: 1,
        dst_channels: 2,
        lpf_direct: 0.0,
        lpf_reverb: 0.0,
        reverb: 0.0,
        doppler: 0.0,
        angle: 0.0,
        distance: 0.0,
        emitter_velocity: 0.0,
        listener_velocity: 0.0,
    };
    X3DAudioCalculate(handle.as_ptr(), &listener, &emitter, MATRIX, &mut s);
    // At -45 degrees (front left): pi/4 from the left speaker, 3pi/4 from the right.
    assert!((matrix[0] - 0.70711).abs() < 1e-5, "{matrix:?}");
    assert!((matrix[1] - 0.29289).abs() < 1e-5, "{matrix:?}");
}

#[test]
fn initialize_fills_the_handle_as_the_console() {
    let mut handle = [0u8; 20];
    X3DAudioInitialize(0x3f, 343.5, handle.as_mut_ptr());
    let h: Handle = unsafe { ptr::read_unaligned(handle.as_ptr().cast()) };
    assert_eq!(
        h,
        Handle {
            mask: 0x3f,
            channels: 6,
            lfe: 3,
            speed: 343.5,
            speed_below: f32::from_bits(343.5f32.to_bits() - 1)
        }
    );
    X3DAudioInitialize(0x3, 343.5, handle.as_mut_ptr());
    let h: Handle = unsafe { ptr::read_unaligned(handle.as_ptr().cast()) };
    assert_eq!((h.channels, h.lfe), (2, NO_LFE));
    // No speakers still counts one, as on the console.
    X3DAudioInitialize(0, 343.5, handle.as_mut_ptr());
    let h: Handle = unsafe { ptr::read_unaligned(handle.as_ptr().cast()) };
    assert_eq!(h.channels, 1);
}

/// Time per call over the vectors (run with `--ignored --nocapture`).
#[test]
#[ignore]
fn profile() {
    let cases = parse(include_str!("vectors.txt"));
    let rounds = 2000;
    let took: std::time::Duration = cases.iter().map(|c| run_with(c, rounds).1).sum();
    let per = took.as_nanos() as f64 / (rounds as usize * cases.len()) as f64;
    eprintln!(
        "{per:.0} ns per X3DAudioCalculate over {} cases",
        cases.len()
    );
}
