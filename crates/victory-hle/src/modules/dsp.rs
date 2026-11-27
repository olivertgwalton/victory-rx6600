//! The console's audio DSP (`chad.dll`): streams that convolve blocks of
//! float samples with an impulse response (reverb), or run other block
//! transforms, on a coprocessor.
//!
//! What the console's `chad.dll` does, and this module does the same:
//!
//! * One device per process. Up to 8 connections share it, and together
//!   they may reserve at most 128 seconds of impulse response
//!   (`DEVICE_LIMIT`, `IR_BUDGET`). `XDspConnect` is
//!   `XDspConnectWithMaximumStreamLimit` with 256 streams and 2 channels.
//! * A connection has `max_streams` stream slots and a pool of 8 KiB pages
//!   for impulse responses, sized from its impulse-response seconds, stream
//!   limit and channel count (`ir_pages`). Activation takes a slot first
//!   (none free: `OUT_OF_RESOURCES`), then checks the description, then
//!   takes the stream's pages (too few: `OUT_OF_RESOURCES`).
//! * A convolution's impulse response is `ir_floats / 2 / frames`
//!   partitions of `frames` packed complex bins each (one set, or two with
//!   option 8); it needs at least 9 partitions and at most 2048 once rounded
//!   up to 8 (`BAD_IR_LENGTH`).
//! * A command's buffers must start inside the memory block given at
//!   connect, or it is refused with `E_INVALIDARG`. A stream holds 8
//!   commands in flight (`QUEUE_FULL`). Submitting bumps the status block's
//!   first word and hands it back as the ticket; the coprocessor writes the
//!   ticket of the last completed command to the second word and a failure
//!   to the fourth, which every later submit returns.
//! * Deactivating a stream with commands in flight fails (`BUSY`), and so
//!   does disconnecting with streams active (`STREAMS_ACTIVE`).
//!
//! Commands complete when submitted here.
//!
//! The transforms themselves are not computed: the coprocessor's firmware
//! does them and is not part of `chad.dll`. Outputs are silence, so reverb
//! returns are dry.
// shortcut: no convolution or block transform; outputs are zeros. Needs the
// coprocessor's spectrum layout (VICTORY_DSP_DUMP captures the title's).
#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::c_void;
use std::sync::Mutex;

use super::xasync::E_INVALIDARG;
use super::xgameruntime::{HRESULT, S_OK};
use crate::{HleModule, hle_export};

const E_POINTER: HRESULT = 0x8000_4003_u32 as i32;
/// All 8 command slots of a stream are in flight.
const QUEUE_FULL: HRESULT = 0x8AC8_10A0_u32 as i32;
/// Deactivating a stream whose commands have not all completed.
const BUSY: HRESULT = 0x8AC8_10A1_u32 as i32;
/// No free stream slot, or too few impulse-response pages.
const OUT_OF_RESOURCES: HRESULT = 0x8AC8_10A2_u32 as i32;
/// Disconnecting while streams are active.
const STREAMS_ACTIVE: HRESULT = 0x8AC8_10A4_u32 as i32;
/// All 8 connections are in use.
const DEVICE_LIMIT: HRESULT = 0x8AC8_10A5_u32 as i32;
/// The connections would reserve more than 128 seconds of impulse response.
const IR_BUDGET: HRESULT = 0x8AC8_10A6_u32 as i32;
/// A convolution with fewer than 9 or more than 2048 partitions.
const BAD_IR_LENGTH: HRESULT = 0x8AC8_10A7_u32 as i32;

const CONVOLUTION: u32 = 1;
/// Option bits that need a stereo stream.
const STEREO_INPUT: u32 = 2;
const NOT_FOR_CONVOLUTION: u32 = 4;
/// A second impulse response for the second channel.
const TWO_IRS: u32 = 8;

const CONNECTIONS: usize = 8;
const IR_SECONDS_TOTAL: u32 = 0x80;
const PAGE: u32 = 0x2000;
const SLOTS_PER_STREAM: u32 = 8;
const SAMPLE_RATE: u32 = 48_000;

/// The device's connections and the seconds of impulse response they hold.
struct Device {
    connections: [usize; CONNECTIONS],
    ir_seconds: u32,
}

static DEVICE: Mutex<Device> = Mutex::new(Device {
    connections: [0; CONNECTIONS],
    ir_seconds: 0,
});

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub struct Dsp {
    base: usize,
    base_length: u32,
    ir_seconds: u32,
    pages: u32,
    state: Mutex<DspState>,
}

struct DspState {
    /// Stream slots, `max_streams` of them.
    slots: Vec<bool>,
    used_pages: u32,
}

/// What activation describes.
#[repr(C, packed)]
pub struct StreamDesc {
    kind: u32,
    options: u32,
    frames: u32,
    channels: u32,
    ir_floats: u32,
    ir: *const f32,
}

/// The status block a stream's owner polls.
#[repr(C)]
pub struct Status {
    submitted: u32,
    completed: u32,
    reserved: u32,
    result: HRESULT,
}

pub struct Stream {
    dsp: *mut Dsp,
    slot: usize,
    kind: u32,
    frames: u32,
    channels: u32,
    partitions: u32,
    pages: u32,
    /// Commands submitted, against which the status block's completed
    /// ticket is checked on deactivation.
    submitted: u32,
    status: Box<Status>,
}

/// One block of work.
#[repr(C)]
pub struct Command {
    gains: [f32; 2],
    input: *const f32,
    output: *mut f32,
    /// A third buffer, for streams of kind 2.
    aux: *const f32,
}

/// The impulse-response pages a connection reserves: the larger of what
/// 512- and 1024-frame convolutions of its seconds need.
fn ir_pages(ir_seconds: u32, max_streams: u32, max_channels: u32) -> u32 {
    if ir_seconds == 0 {
        return 0;
    }
    let samples = ir_seconds * SAMPLE_RATE;
    let mut bytes = 0u32;
    for (frames, divisor, per_stream) in [(0x200u32, 0x1200u32, 0x13000u32), (0x400, 0x2400, 0x22000)]
    {
        let streams = (samples / divisor).min(max_streams);
        let need = (per_stream
            .wrapping_mul(streams)
            .wrapping_mul(max_channels)
            .wrapping_add(PAGE - 1)
            .wrapping_add((samples - samples % frames) * 16))
            & !(PAGE - 1);
        bytes = bytes.max(need);
    }
    bytes.div_ceil(PAGE)
}

/// Partitions of a convolution and the pages they take, or why it is refused.
fn convolution_pages(desc: &StreamDesc) -> Result<(u32, u32), HRESULT> {
    let (options, frames, channels, ir_floats) =
        (desc.options, desc.frames, desc.channels, desc.ir_floats);
    let partitions = (ir_floats / 2) / frames;
    let rounded = partitions.next_multiple_of(8);
    if partitions < 9 || rounded > 0x800 {
        return Err(BAD_IR_LENGTH);
    }
    let two = options & TWO_IRS != 0;
    let bytes = ((if two { 16 } else { 8 }) * partitions + (rounded + 8) * channels * 8) * frames;
    Ok((
        partitions,
        bytes.div_ceil(PAGE) + channels + if two { 2 } else { 1 },
    ))
}

pub extern "system" fn XDspConnectWithMaximumStreamLimit(
    base: *mut c_void,
    base_length: u32,
    ir_seconds: u32,
    max_streams: u32,
    max_channels: u32,
    out: *mut *mut Dsp,
) -> HRESULT {
    if base.is_null()
        || !(0x1_0000..=0x1F40_0000).contains(&base_length)
        || ir_seconds > 0x80
        || max_streams > 0x100
        || max_channels > 2
    {
        return E_INVALIDARG;
    }
    if out.is_null() {
        return E_POINTER;
    }
    let mut device = lock(&DEVICE);
    if device.ir_seconds + ir_seconds > IR_SECONDS_TOTAL {
        return IR_BUDGET;
    }
    let Some(index) = device.connections.iter().position(|&c| c == 0) else {
        return DEVICE_LIMIT;
    };
    let pages = ir_pages(ir_seconds, max_streams, max_channels);
    tracing::info!(
        base_length,
        ir_seconds,
        max_streams,
        max_channels,
        pages,
        "audio DSP connected"
    );
    let dsp = Box::into_raw(Box::new(Dsp {
        base: base as usize,
        base_length,
        ir_seconds,
        pages,
        state: Mutex::new(DspState {
            slots: vec![false; max_streams as usize],
            used_pages: 0,
        }),
    }));
    device.connections[index] = dsp as usize;
    device.ir_seconds += ir_seconds;
    // SAFETY: checked non-null.
    unsafe { out.write(dsp) };
    S_OK
}

/// The same with 256 streams of up to 2 channels.
pub extern "system" fn XDspConnect(
    base: *mut c_void,
    base_length: u32,
    ir_seconds: u32,
    out: *mut *mut Dsp,
) -> HRESULT {
    XDspConnectWithMaximumStreamLimit(base, base_length, ir_seconds, 0x100, 2, out)
}

pub extern "system" fn XDspDisconnect(dsp: *mut Dsp) -> HRESULT {
    let mut device = lock(&DEVICE);
    let Some(index) = (!dsp.is_null())
        .then(|| device.connections.iter().position(|&c| c == dsp as usize))
        .flatten()
    else {
        return E_INVALIDARG;
    };
    {
        // SAFETY: one of our connections.
        let state = lock(unsafe { &(*dsp).state });
        if state.slots.iter().any(|&s| s) {
            return STREAMS_ACTIVE;
        }
    }
    device.connections[index] = 0;
    // SAFETY: made by `XDspConnectWithMaximumStreamLimit`, now off the list.
    let dsp = unsafe { Box::from_raw(dsp) };
    device.ir_seconds -= dsp.ir_seconds;
    S_OK
}

fn valid(desc: &StreamDesc) -> bool {
    let (kind, options, frames, channels) = (desc.kind, desc.options, desc.frames, desc.channels);
    let shape = if kind == CONVOLUTION {
        (1..=2).contains(&channels) && matches!(frames, 0x200 | 0x400)
    } else {
        channels == 1 && matches!(frames, 0x200 | 0x400 | 0x800)
    };
    shape
        && (options & STEREO_INPUT == 0 || channels == 2)
        && (options & NOT_FOR_CONVOLUTION == 0 || kind != CONVOLUTION)
        && (options & TWO_IRS == 0 || channels == 2)
}

pub extern "system" fn XDspActivate(
    dsp: *mut Dsp,
    desc: *const StreamDesc,
    status: *mut *const Status,
    stream: *mut *mut Stream,
) -> HRESULT {
    let hr = activate(dsp, desc, status, stream);
    if hr != S_OK && !desc.is_null() {
        // SAFETY: the title's description.
        let d = unsafe { &*desc };
        let (kind, options, frames, channels, ir_floats) =
            (d.kind, d.options, d.frames, d.channels, d.ir_floats);
        tracing::warn!(
            hr = format_args!("{:#x}", hr as u32),
            kind,
            options,
            frames,
            channels,
            ir_floats,
            "audio DSP stream refused"
        );
    }
    hr
}

fn activate(
    dsp: *mut Dsp,
    desc: *const StreamDesc,
    status: *mut *const Status,
    stream: *mut *mut Stream,
) -> HRESULT {
    if status.is_null() || stream.is_null() {
        return E_POINTER;
    }
    if dsp.is_null() || desc.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: a connection from `XDspConnectWithMaximumStreamLimit`; the
    // title's description.
    let (connection, desc) = unsafe { (&*dsp, &*desc) };
    let mut state = lock(&connection.state);
    // A slot first, as the console does: a full connection refuses even a
    // bad description.
    let Some(slot) = state.slots.iter().position(|&s| !s) else {
        return OUT_OF_RESOURCES;
    };
    if !valid(desc) {
        return E_INVALIDARG;
    }
    let (kind, options, frames, channels, ir_floats) = (
        desc.kind,
        desc.options,
        desc.frames,
        desc.channels,
        desc.ir_floats,
    );
    let (partitions, pages) = if kind == CONVOLUTION {
        match convolution_pages(desc) {
            Ok(p) => p,
            Err(e) => return e,
        }
    } else {
        (0, 0)
    };
    if connection.pages - state.used_pages < pages {
        return OUT_OF_RESOURCES;
    }
    state.slots[slot] = true;
    state.used_pages += pages;
    drop(state);
    log_activation(desc, partitions);
    tracing::debug!(
        kind,
        options,
        frames,
        channels,
        ir_floats,
        partitions,
        pages,
        "audio DSP stream activated"
    );
    let new = Box::new(Stream {
        dsp,
        slot,
        kind,
        frames,
        channels,
        partitions,
        pages,
        submitted: 0,
        status: Box::new(Status {
            submitted: 0,
            completed: 0,
            reserved: 0,
            result: S_OK,
        }),
    });
    // SAFETY: checked non-null.
    unsafe {
        status.write(&*new.status);
        stream.write(Box::into_raw(new));
    }
    S_OK
}

pub extern "system" fn XDspDeactivate(stream: *mut Stream) -> HRESULT {
    if stream.is_null() {
        return E_INVALIDARG;
    }
    {
        // SAFETY: made by `XDspActivate`.
        let s = unsafe { &*stream };
        // SAFETY: the status block is written by the coprocessor's side.
        let completed = unsafe { std::ptr::read_volatile(&s.status.completed) };
        if s.submitted != completed {
            return BUSY;
        }
    }
    // SAFETY: made by `XDspActivate`; deactivated once.
    let stream = unsafe { Box::from_raw(stream) };
    // SAFETY: the connection outlives its streams.
    let dsp = unsafe { &*stream.dsp };
    let mut state = lock(&dsp.state);
    state.slots[stream.slot] = false;
    state.used_pages -= stream.pages;
    S_OK
}

impl Dsp {
    /// Whether `p` starts inside the memory given at connect.
    fn holds(&self, p: *const f32) -> bool {
        (p as usize).wrapping_sub(self.base) < self.base_length as usize
    }
}

fn submit(
    stream: *mut Stream,
    command: *const Command,
    envelope: *const f32,
    ticket: *mut u32,
) -> HRESULT {
    if command.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: a stream from `XDspActivate`, the title's command.
    let (stream, command) = unsafe { (&mut *stream, &*command) };
    // SAFETY: the connection outlives its streams.
    let dsp = unsafe { &*stream.dsp };
    // Written here, read by title code on other threads.
    let status: *mut Status = &mut *stream.status;
    // SAFETY: the stream's status block.
    let failed = unsafe { std::ptr::read_volatile(&raw const (*status).result) };
    if failed < 0 {
        return failed;
    }
    // SAFETY: as above.
    let completed = unsafe { std::ptr::read_volatile(&raw const (*status).completed) };
    if stream.submitted.wrapping_sub(completed) >= SLOTS_PER_STREAM {
        return QUEUE_FULL;
    }
    if stream.kind == CONVOLUTION && !envelope.is_null() && !dsp.holds(envelope) {
        return E_INVALIDARG;
    }
    if !dsp.holds(command.input)
        || !dsp.holds(command.output)
        || (stream.kind == 2 && !command.aux.is_null() && !dsp.holds(command.aux))
    {
        return E_INVALIDARG;
    }
    log_command(stream, command);
    let samples = (stream.frames * stream.channels) as usize;
    // SAFETY: the title's output buffer holds one block of the stream.
    unsafe { std::ptr::write_bytes(command.output, 0, samples) };
    stream.submitted = stream.submitted.wrapping_add(1);
    // SAFETY: the stream's status block.
    let next = unsafe { std::ptr::read_volatile(&raw const (*status).submitted) }.wrapping_add(1);
    // SAFETY: as above.
    unsafe {
        std::ptr::write_volatile(&raw mut (*status).submitted, next);
        std::ptr::write_volatile(&raw mut (*status).result, S_OK);
        std::ptr::write_volatile(&raw mut (*status).completed, next);
    }
    if !ticket.is_null() {
        // SAFETY: optional out parameter.
        unsafe { ticket.write(next) };
    }
    S_OK
}

/// Completes the command at once: its output block is silence.
pub extern "system" fn XDspSubmitCommand(
    stream: *mut Stream,
    command: *const Command,
    ticket: *mut u32,
) -> HRESULT {
    if stream.is_null() {
        return E_INVALIDARG;
    }
    submit(stream, command, std::ptr::null(), ticket)
}

/// The same, with per-partition gains for a convolution's impulse response.
pub extern "system" fn XDspSubmitCommandWithEnvelope(
    stream: *mut Stream,
    command: *const Command,
    envelope: *const f32,
    ticket: *mut u32,
) -> HRESULT {
    if stream.is_null() {
        return E_INVALIDARG;
    }
    submit(stream, command, envelope, ticket)
}

// Capture of what the title activates and submits (VICTORY_DSP_DUMP=<dir>):
// descriptions and the first commands go to the log, impulse responses to
// `<dir>/dsp-ir-<n>.f32`.

static ACTIVATIONS: Mutex<u32> = Mutex::new(0);
static COMMANDS: Mutex<u32> = Mutex::new(0);
const LOGGED: u32 = 16;

fn log_activation(desc: &StreamDesc, partitions: u32) {
    let mut n = lock(&ACTIVATIONS);
    *n += 1;
    if *n > LOGGED {
        return;
    }
    let (kind, options, frames, channels, ir_floats, ir) = (
        desc.kind,
        desc.options,
        desc.frames,
        desc.channels,
        desc.ir_floats,
        desc.ir,
    );
    tracing::info!(
        n = *n,
        kind,
        options,
        frames,
        channels,
        ir_floats,
        partitions,
        "audio DSP stream"
    );
    let Some(dir) = std::env::var_os("VICTORY_DSP_DUMP") else {
        return;
    };
    if kind != CONVOLUTION || ir.is_null() {
        return;
    }
    let floats = ir_floats as usize * if options & TWO_IRS != 0 { 2 } else { 1 };
    // SAFETY: the title's impulse response, `ir_floats` per set.
    let data = unsafe { std::slice::from_raw_parts(ir.cast::<u8>(), floats * 4) };
    let path = std::path::Path::new(&dir).join(format!("dsp-ir-{}.f32", *n));
    if let Err(e) = std::fs::write(&path, data) {
        tracing::warn!(%e, path = %path.display(), "audio DSP: impulse response not saved");
    }
}

fn log_command(stream: &Stream, command: &Command) {
    let mut n = lock(&COMMANDS);
    if *n >= LOGGED || std::env::var_os("VICTORY_DSP_DUMP").is_none() {
        return;
    }
    *n += 1;
    let samples = (stream.frames * stream.channels) as usize;
    // SAFETY: the title's input block.
    let input = unsafe { std::slice::from_raw_parts(command.input, samples) };
    let energy = input.iter().map(|s| s * s).sum::<f32>() / samples as f32;
    // Neighbouring samples: alike for planar blocks, less so interleaved.
    let adjacent = input.windows(2).map(|w| (w[0] - w[1]).abs()).sum::<f32>() / samples as f32;
    let paired = input.windows(3).map(|w| (w[0] - w[2]).abs()).sum::<f32>() / samples as f32;
    let first: Vec<f32> = input.iter().take(6).copied().collect();
    tracing::info!(
        n = *n,
        kind = stream.kind,
        partitions = stream.partitions,
        gains = ?command.gains,
        in_place = std::ptr::eq(command.input, command.output),
        energy,
        adjacent,
        paired,
        ?first,
        "audio DSP command"
    );
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "dsp",
        dlls: &["chad.dll"],
        exports: vec![
            hle_export!(XDspConnect),
            hle_export!(XDspConnectWithMaximumStreamLimit),
            hle_export!(XDspDisconnect),
            hle_export!(XDspActivate),
            hle_export!(XDspDeactivate),
            hle_export!(XDspSubmitCommand),
            hle_export!(XDspSubmitCommandWithEnvelope),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The device is one per process: tests that connect run one at a time.
    static SERIAL: Mutex<()> = Mutex::new(());

    fn connect(base: &mut [u8], ir_seconds: u32, max_streams: u32) -> (HRESULT, *mut Dsp) {
        let mut dsp = std::ptr::null_mut();
        let hr = XDspConnectWithMaximumStreamLimit(
            base.as_mut_ptr().cast(),
            base.len() as u32,
            ir_seconds,
            max_streams,
            1,
            &mut dsp,
        );
        (hr, dsp)
    }

    #[test]
    fn convolution_stream_as_the_title_drives_it() {
        let _serial = lock(&SERIAL);
        let mut base = vec![0u8; 0x10_0000];
        let (hr, dsp) = connect(&mut base, 30, 0x80);
        assert_eq!(hr, S_OK);
        // 30 s, 128 streams, mono: the title's connection.
        // SAFETY: just connected.
        assert_eq!(unsafe { (*dsp).pages }, 4988);
        // 16 partitions of 1024 stereo bins.
        let ir = vec![0f32; 16 * 0x800];
        let desc = StreamDesc {
            kind: CONVOLUTION,
            options: STEREO_INPUT,
            frames: 0x400,
            channels: 2,
            ir_floats: 16 * 0x800,
            ir: ir.as_ptr(),
        };
        let (mut status, mut stream) = (std::ptr::null(), std::ptr::null_mut());
        assert_eq!(XDspActivate(dsp, &desc, &mut status, &mut stream), S_OK);
        let block = base.as_mut_ptr().cast::<f32>();
        // SAFETY: inside `base`.
        unsafe { std::slice::from_raw_parts_mut(block, 0x800).fill(1.0) };
        let command = Command {
            gains: [1.0, 1.0],
            input: block.wrapping_add(0x1000),
            output: block,
            aux: std::ptr::null(),
        };
        let mut ticket = 0;
        assert_eq!(XDspSubmitCommand(stream, &command, &mut ticket), S_OK);
        assert_eq!(ticket, 1);
        // SAFETY: the stream's status block.
        let (submitted, completed, result) =
            unsafe { ((*status).submitted, (*status).completed, (*status).result) };
        assert_eq!((submitted, completed, result), (1, 1, S_OK));
        // SAFETY: inside `base`.
        assert!(unsafe { std::slice::from_raw_parts(block, 0x800) }.iter().all(|&s| s == 0.0));

        // Buffers outside the connection's memory are refused.
        let outside = vec![0f32; 0x800];
        let stray = Command {
            input: outside.as_ptr(),
            ..command
        };
        assert_eq!(XDspSubmitCommand(stream, &stray, &mut ticket), E_INVALIDARG);

        // Streams active: the connection stays.
        assert_eq!(XDspDisconnect(dsp), STREAMS_ACTIVE);
        assert_eq!(XDspDeactivate(stream), S_OK);

        // Too short an impulse response, and stereo options on mono.
        let short = StreamDesc {
            ir_floats: 8 * 0x800,
            ..desc
        };
        assert_eq!(
            XDspActivate(dsp, &short, &mut status, &mut stream),
            BAD_IR_LENGTH
        );
        let mono = StreamDesc {
            channels: 1,
            ..desc
        };
        assert_eq!(
            XDspActivate(dsp, &mono, &mut status, &mut stream),
            E_INVALIDARG
        );
        // Transforms are mono and need no pages.
        let transform = StreamDesc {
            kind: 2,
            options: 0,
            channels: 1,
            ir_floats: 0,
            ir: std::ptr::null(),
            ..desc
        };
        assert_eq!(
            XDspActivate(dsp, &transform, &mut status, &mut stream),
            S_OK
        );
        assert_eq!(XDspDeactivate(stream), S_OK);
        assert_eq!(XDspDisconnect(dsp), S_OK);
        assert_eq!(XDspDisconnect(dsp), E_INVALIDARG);
    }

    #[test]
    fn device_limits_connections_and_seconds() {
        let _serial = lock(&SERIAL);
        let mut base = vec![0u8; 0x1_0000];
        let (hr, a) = connect(&mut base, 100, 4);
        assert_eq!(hr, S_OK);
        assert_eq!(connect(&mut base, 29, 4).0, IR_BUDGET);
        let (hr, b) = connect(&mut base, 28, 4);
        assert_eq!(hr, S_OK);
        assert_eq!(XDspDisconnect(b), S_OK);
        // Seven more at no seconds, then the device is full.
        let more: Vec<_> = (0..7).map(|_| connect(&mut base, 0, 1)).collect();
        assert!(more.iter().all(|(hr, _)| *hr == S_OK));
        assert_eq!(connect(&mut base, 0, 1).0, DEVICE_LIMIT);
        for (_, d) in more {
            assert_eq!(XDspDisconnect(d), S_OK);
        }

        // One stream slot: the second activation is refused before its
        // description is looked at.
        let mut base = vec![0u8; 0x1_0000];
        let (hr, one) = connect(&mut base, 0, 1);
        assert_eq!(hr, S_OK);
        let desc = StreamDesc {
            kind: 2,
            options: 0,
            frames: 0x800,
            channels: 1,
            ir_floats: 0,
            ir: std::ptr::null(),
        };
        let (mut status, mut stream) = (std::ptr::null(), std::ptr::null_mut());
        assert_eq!(XDspActivate(one, &desc, &mut status, &mut stream), S_OK);
        let bad = StreamDesc { channels: 2, ..desc };
        let mut other = std::ptr::null_mut();
        assert_eq!(
            XDspActivate(one, &bad, &mut status, &mut other),
            OUT_OF_RESOURCES
        );
        assert_eq!(XDspDeactivate(stream), S_OK);
        assert_eq!(XDspDisconnect(one), S_OK);
        assert_eq!(XDspDisconnect(a), S_OK);
    }
}
