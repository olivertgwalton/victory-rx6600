//! The console's audio decoder (`xapu.dll`): Opus packets in, float PCM out.
//!
//! A title connects once for one kind of processing, gets a block of memory
//! for its packets and output, and then queues per-stream commands
//! (activate, process, reset, deactivate) and dequeues one result per
//! command. We decode each command when it is queued, so its result is ready
//! at once; the order of results is the order of commands.
//!
//! What the console's `xapu.dll` checks and returns, and this module too:
//!
//! * Connect: a null handle or output pointer is `E_POINTER` (both are
//!   cleared first), a null input `E_INVALIDARG`. Processing kinds 1-3,
//!   1-500 streams, 1-16 commands per stream, option bits 0-3 only, and for
//!   rate conversion (kinds 2, 3) an even 128-1024 frames per output; more
//!   than 512 MiB of memory is `E_OUTOFMEMORY`. All connections share 500
//!   streams (`OUT_OF_STREAMS`). The handle is the connection's address
//!   inverted.
//! * Enqueue: activate needs an inactive stream (`ALREADY_ACTIVE`), the
//!   others an active one; a stream out of range or not active is
//!   `INVALID_STREAM`, a stream with all its commands in flight
//!   `QUEUE_FULL`, any other command kind `E_INVALIDARG`. These come back
//!   from the call, not in a result. A stream stays active until the result
//!   of its deactivation is dequeued. The out count is commands queued so
//!   far.
//! * Results: a process result is the coprocessor's whole record; any other
//!   result writes only the command's id, the time taken (microseconds) and
//!   the error, and leaves the rest of the caller's record as it was.
//! * A packet or output buffer is passed as its address in the connection's
//!   memory; one that starts outside it reaches the decoder as none. A
//!   process with no packet decodes the next frames as lost (concealment).
//! * Disconnect returns `PENDING_RESULTS` when results were left
//!   undequeued, and disconnects all the same.
//! * `XApuGetUtilization` reports the streams left of the 500, and 500.
#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::alloc::Layout;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::Mutex;
use std::time::Instant;

use opus_pure::OpusDecoder;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::Threading::{CreateEventW, SetEvent};

use super::xasync::E_INVALIDARG;
use super::xgameruntime::{HRESULT, S_OK};
use crate::{HleModule, hle_export};

const E_NOTIMPL: HRESULT = 0x8000_4001_u32 as i32;
const E_POINTER: HRESULT = 0x8000_4003_u32 as i32;
const E_OUTOFMEMORY: HRESULT = 0x8007_000E_u32 as i32;
const QUEUE_FULL: HRESULT = 0x8AC8_1000_u32 as i32;
/// No result yet, which titles poll for.
pub(crate) const PENDING_RESULTS: HRESULT = 0x8AC8_1001_u32 as i32;
const INVALID_STREAM: HRESULT = 0x8AC8_1003_u32 as i32;
const OUT_OF_STREAMS: HRESULT = 0x8AC8_1004_u32 as i32;
const ALREADY_ACTIVE: HRESULT = 0x8AC8_1005_u32 as i32;
const BAD_PACKET: HRESULT = 0x8AC8_1008_u32 as i32;

/// Processing kinds; only plain decoding is implemented.
const DECODE_OPUS: u32 = 1;

const ACTIVATE: u32 = 1;
const DEACTIVATE: u32 = 2;
const PROCESS: u32 = 3;
const RESET: u32 = 5;

/// Command option: signal the connection's event when the result is ready.
const SIGNAL: u32 = 1;
/// Connect options.
const DEINTERLEAVED_OUTPUT: u32 = 4;
const RESULT_EVENT: u32 = 8;

/// Streams of all connections together.
const STREAMS: u32 = 500;
static STREAMS_USED: Mutex<u32> = Mutex::new(0);

/// The longest Opus packet duration at 48 kHz (120 ms).
const MAX_FRAMES: usize = 5760;

#[repr(C)]
pub struct ConnectInput {
    process: u32,
    streams: u32,
    commands_per_stream: u32,
    memory: u32,
    frames_per_output: u32,
    options: u32,
}

#[repr(C)]
pub struct ConnectOutput {
    memory: *mut c_void,
    result_event: HANDLE,
}

/// What every command starts with, and what its result echoes.
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct CommandId {
    kind: u32,
    stream: u32,
    sequence: u32,
    options: u32,
    context: u64,
}

#[repr(C, packed)]
struct Activate {
    id: CommandId,
    channels: u32,
    init_length: u32,
    init: *const c_void,
    start_pitch: f32,
    processing_length: u32,
    processing: *mut c_void,
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct Process {
    id: CommandId,
    pitch_ramp: f32,
    target_pitch: f32,
    first_frame: u32,
    frame_count: u32,
    input_length: u32,
    input: *const u8,
    output_length: u32,
    output: *mut u8,
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct ApuResult {
    id: CommandId,
    state: u32,
    state_data: u64,
    usec: u64,
    error: HRESULT,
    frames: u32,
    output_length: u32,
    output: *mut u8,
    pitch: f32,
    input_length: u32,
    input: *const u8,
}

// SAFETY: the raw pointers are title addresses carried through, never
// dereferenced after the result is made.
unsafe impl Send for ApuResult {}

/// A result waiting to be dequeued.
struct Done {
    result: ApuResult,
    /// A process result is written whole; others only in part.
    whole: bool,
}

pub struct Connection {
    memory: *mut u8,
    layout: Layout,
    event: HANDLE,
    options: u32,
    per_stream: u32,
    streams: u32,
    state: Mutex<State>,
}

struct State {
    /// Active streams (by the commands queued), and their decoders.
    active: Vec<bool>,
    decoders: Vec<Option<OpusDecoder>>,
    /// Results not yet dequeued, per stream.
    queued: Vec<u32>,
    results: VecDeque<Done>,
    enqueued: u32,
    dequeued: u32,
    /// Decoded samples before they are cut to the command's frames.
    pcm: Vec<f32>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The connection a handle names: its address inverted.
fn connection<'a>(handle: *mut Connection) -> Option<&'a Connection> {
    if handle.is_null() {
        return None;
    }
    // SAFETY: a handle from `XApuConnect`, not yet disconnected.
    Some(unsafe { &*(!(handle as usize) as *const Connection) })
}

impl Connection {
    /// A title address as the coprocessor sees it: null unless it starts
    /// inside the connection's memory.
    fn inside<T>(&self, p: *const T) -> *const T {
        let offset = (p as usize).wrapping_sub(self.memory as usize);
        if offset < self.layout.size() {
            p
        } else {
            std::ptr::null()
        }
    }
}

pub extern "system" fn XApuConnect(
    input: *const ConnectInput,
    output: *mut ConnectOutput,
    handle: *mut *mut Connection,
) -> HRESULT {
    if handle.is_null() {
        return E_POINTER;
    }
    // SAFETY: checked non-null.
    unsafe { handle.write(std::ptr::null_mut()) };
    if output.is_null() {
        return E_POINTER;
    }
    // SAFETY: checked non-null.
    unsafe {
        output.write(ConnectOutput {
            memory: std::ptr::null_mut(),
            result_event: std::ptr::null_mut(),
        })
    };
    if input.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: checked non-null.
    let input = unsafe { &*input };
    if !(1..=3).contains(&input.process)
        || !(1..=STREAMS).contains(&input.streams)
        || !(1..=16).contains(&input.commands_per_stream)
    {
        return E_INVALIDARG;
    }
    if input.memory > 0x2000_0000 {
        return E_OUTOFMEMORY;
    }
    if input.options & !0xF != 0
        || (input.process != DECODE_OPUS
            && (!(0x80..=0x400).contains(&input.frames_per_output)
                || input.frames_per_output % 2 != 0))
    {
        return E_INVALIDARG;
    }
    let mut used = lock(&STREAMS_USED);
    if input.streams > STREAMS - *used {
        return OUT_OF_STREAMS;
    }
    if input.process != DECODE_OPUS {
        // shortcut: rate conversion (decode-convert, PCM convert) is not
        // implemented; add a resampler if a title connects for it.
        tracing::warn!(
            process = input.process,
            "audio decoder: processing kind not implemented"
        );
        return E_NOTIMPL;
    }
    // The console carves the block from its own allocation; one of none
    // fails like an allocation would.
    let Ok(layout) = Layout::from_size_align(input.memory as usize, 16) else {
        return E_OUTOFMEMORY;
    };
    if input.memory == 0 {
        return E_OUTOFMEMORY;
    }
    // SAFETY: non-zero size.
    let memory = unsafe { std::alloc::alloc_zeroed(layout) };
    if memory.is_null() {
        return E_OUTOFMEMORY;
    }
    let event = if input.options & RESULT_EVENT != 0 {
        // SAFETY: plain auto-reset event, no name.
        let event = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
        if event.is_null() {
            // SAFETY: allocated above with `layout`.
            unsafe { std::alloc::dealloc(memory, layout) };
            return E_OUTOFMEMORY;
        }
        event
    } else {
        std::ptr::null_mut()
    };
    *used += input.streams;
    let streams = input.streams as usize;
    tracing::info!(
        streams,
        memory = input.memory,
        options = input.options,
        "audio decoder connected"
    );
    let connection = Box::new(Connection {
        memory,
        layout,
        event,
        options: input.options,
        per_stream: input.commands_per_stream,
        streams: input.streams,
        state: Mutex::new(State {
            active: vec![false; streams],
            decoders: (0..streams).map(|_| None).collect(),
            queued: vec![0; streams],
            results: VecDeque::new(),
            enqueued: 0,
            dequeued: 0,
            pcm: vec![0.0; MAX_FRAMES * 2],
        }),
    });
    // SAFETY: checked non-null.
    unsafe {
        output.write(ConnectOutput {
            memory: memory.cast(),
            result_event: event,
        });
        handle.write(!(Box::into_raw(connection) as usize) as *mut Connection);
    }
    S_OK
}

pub extern "system" fn XApuDisconnect(handle: *mut Connection) -> HRESULT {
    if handle.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: made by `XApuConnect`; disconnected once.
    let connection = unsafe { Box::from_raw(!(handle as usize) as *mut Connection) };
    let hr = {
        let state = lock(&connection.state);
        let ready = state.results.len() as u32;
        if state.dequeued.wrapping_add(ready) < state.enqueued {
            PENDING_RESULTS
        } else {
            S_OK
        }
    };
    *lock(&STREAMS_USED) -= connection.streams;
    if !connection.event.is_null() {
        // SAFETY: our event.
        unsafe { CloseHandle(connection.event) };
    }
    // SAFETY: allocated in `XApuConnect` with this layout.
    unsafe { std::alloc::dealloc(connection.memory, connection.layout) };
    hr
}

pub extern "system" fn XApuEnqueueCommand(
    handle: *mut Connection,
    command: *const CommandId,
    total: *mut u32,
) -> HRESULT {
    let Some(connection) = connection(handle) else {
        return E_INVALIDARG;
    };
    if command.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: every command starts with an id.
    let id = unsafe { command.read_unaligned() };
    if !matches!(id.kind, ACTIVATE | DEACTIVATE | PROCESS | RESET) {
        return E_INVALIDARG;
    }
    let started = Instant::now();
    let mut state = lock(&connection.state);
    let stream = id.stream as usize;
    if id.stream >= connection.streams {
        return INVALID_STREAM;
    }
    match (id.kind, state.active[stream]) {
        (ACTIVATE, true) => return ALREADY_ACTIVE,
        (ACTIVATE, false) => {}
        (_, false) => return INVALID_STREAM,
        _ => {}
    }
    if state.queued[stream] >= connection.per_stream {
        return QUEUE_FULL;
    }
    let mut result = ApuResult {
        id,
        state: 0,
        state_data: 0,
        usec: 0,
        error: S_OK,
        frames: 0,
        output_length: 0,
        output: std::ptr::null_mut(),
        pitch: 1.0,
        input_length: 0,
        input: std::ptr::null(),
    };
    match id.kind {
        ACTIVATE => {
            state.active[stream] = true;
            // SAFETY: an activate command is this struct.
            let channels = unsafe { command.cast::<Activate>().read_unaligned() }.channels;
            tracing::debug!(stream, channels, "Opus stream activated");
            result.error = if !(1..=2).contains(&channels) {
                E_INVALIDARG
            } else {
                match OpusDecoder::new(48_000, channels as usize) {
                    Ok(d) => {
                        state.decoders[stream] = Some(d);
                        S_OK
                    }
                    Err(_) => E_INVALIDARG,
                }
            };
        }
        DEACTIVATE => state.decoders[stream] = None,
        RESET => {
            if let Some(d) = &mut state.decoders[stream] {
                let _ = d.reset_state();
            }
        }
        _ => {
            // SAFETY: a process command is this struct.
            let mut command = unsafe { command.cast::<Process>().read_unaligned() };
            command.input = connection.inside(command.input);
            command.output = connection.inside(command.output).cast_mut();
            let State { decoders, pcm, .. } = &mut *state;
            match &mut decoders[stream] {
                Some(d) => process(d, connection.options, &command, &mut result, pcm),
                None => result.error = INVALID_STREAM,
            }
        }
    }
    result.usec = started.elapsed().as_micros() as u64;
    state.queued[stream] += 1;
    state.enqueued = state.enqueued.wrapping_add(1);
    state.results.push_back(Done {
        result,
        whole: id.kind == PROCESS,
    });
    if !total.is_null() {
        // SAFETY: optional out parameter.
        unsafe { total.write(state.enqueued) };
    }
    if id.options & SIGNAL != 0 && !connection.event.is_null() {
        // SAFETY: our event.
        unsafe { SetEvent(connection.event) };
    }
    S_OK
}

/// Decodes one packet into the command's output buffer, keeping the frames
/// from `first_frame` on, at most `frame_count` of them. No packet decodes
/// the frames as lost.
fn process(
    decoder: &mut OpusDecoder,
    options: u32,
    command: &Process,
    result: &mut ApuResult,
    pcm: &mut [f32],
) {
    let channels = decoder.channels();
    let (input, input_length) = (command.input, command.input_length);
    let (output, output_length) = (command.output, command.output_length);
    result.input = input;
    result.input_length = input_length;
    result.output = output;
    if output.is_null() {
        result.error = E_INVALIDARG;
        return;
    }
    let packet = if input.is_null() || input_length == 0 {
        &[][..]
    } else {
        // SAFETY: the title's packet in its connection memory.
        unsafe { std::slice::from_raw_parts(input, input_length as usize) }
    };
    let pcm = &mut pcm[..MAX_FRAMES * channels];
    // A lost packet is concealed for as many frames as asked for.
    let size = if packet.is_empty() {
        (command.first_frame as usize + command.frame_count as usize).clamp(120, MAX_FRAMES)
    } else {
        MAX_FRAMES
    };
    let decoded = match decoder.decode(packet, size, pcm) {
        Ok(n) => n,
        Err(_) => {
            result.error = BAD_PACKET;
            return;
        }
    };
    let first = (command.first_frame as usize).min(decoded);
    let frames = (decoded - first)
        .min(command.frame_count as usize)
        .min(output_length as usize / (4 * channels));
    let kept = &pcm[first * channels..(first + frames) * channels];
    let bytes = frames * channels * 4;
    if bytes > 0 {
        // SAFETY: `output` is non-null and holds at least `bytes <= output_length` bytes;
        // writing raw bytes avoids requiring 4-byte alignment on the title's `*mut u8`.
        let out = unsafe { std::slice::from_raw_parts_mut(output, bytes) };
        if options & DEINTERLEAVED_OUTPUT != 0 && channels == 2 {
            let (left, right) = out.split_at_mut(frames * 4);
            let (left, _) = left.as_chunks_mut::<4>();
            let (right, _) = right.as_chunks_mut::<4>();
            for (i, frame) in kept.as_chunks::<2>().0.iter().enumerate() {
                left[i] = frame[0].to_le_bytes();
                right[i] = frame[1].to_le_bytes();
            }
        } else {
            for (dst, &sample) in out.as_chunks_mut::<4>().0.iter_mut().zip(kept) {
                *dst = sample.to_le_bytes();
            }
        }
    }
    result.frames = frames as u32;
    result.output_length = bytes as u32;
}

pub extern "system" fn XApuDequeueResult(handle: *mut Connection, out: *mut ApuResult) -> HRESULT {
    let Some(connection) = connection(handle) else {
        return E_INVALIDARG;
    };
    if out.is_null() {
        return E_INVALIDARG;
    }
    let mut state = lock(&connection.state);
    let Some(Done { result, whole }) = state.results.pop_front() else {
        return PENDING_RESULTS;
    };
    let stream = result.id.stream as usize;
    state.queued[stream] -= 1;
    if result.id.kind == DEACTIVATE {
        state.active[stream] = false;
    }
    state.dequeued = state.dequeued.wrapping_add(1);
    if whole {
        // SAFETY: the title passes room for one result.
        unsafe { out.write_unaligned(result) };
    } else {
        let base = out.cast::<u8>();
        // SAFETY: the title passes room for one result; only these fields
        // of it are written.
        unsafe {
            base.cast::<CommandId>().write_unaligned(result.id);
            base.add(0x24).cast::<u64>().write_unaligned(result.usec);
            base.add(0x2c).cast::<HRESULT>().write_unaligned(result.error);
        }
    }
    S_OK
}

pub extern "system" fn XApuGetResultCount(handle: *mut Connection, total: *mut u32) -> u32 {
    let Some(connection) = connection(handle) else {
        return 0;
    };
    let state = lock(&connection.state);
    if !total.is_null() {
        // SAFETY: optional out parameter.
        unsafe { total.write(state.dequeued) };
    }
    state.results.len() as u32
}

/// Streams left for connections of a processing kind, and how many there
/// are in all.
#[repr(C)]
pub struct Utilization {
    available: u32,
    total: u32,
}

pub extern "system" fn XApuGetUtilization(process: u32, unit: u32, out: *mut Utilization) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    // SAFETY: checked non-null.
    unsafe {
        out.write(Utilization {
            available: 0,
            total: 0,
        })
    };
    if !(1..=3).contains(&process) || !(1..=2).contains(&unit) {
        return E_INVALIDARG;
    }
    let used = *lock(&STREAMS_USED);
    // SAFETY: checked non-null.
    unsafe {
        out.write(Utilization {
            available: STREAMS - used,
            total: STREAMS,
        })
    };
    S_OK
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "apu",
        dlls: &["xapu.dll"],
        exports: vec![
            hle_export!(XApuConnect),
            hle_export!(XApuDisconnect),
            hle_export!(XApuEnqueueCommand),
            hle_export!(XApuDequeueResult),
            hle_export!(XApuGetResultCount),
            hle_export!(XApuGetUtilization),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opus_pure::{Application, OpusEncoder};

    fn id(kind: u32, stream: u32) -> CommandId {
        CommandId {
            kind,
            stream,
            sequence: 7,
            options: 0,
            context: 0x1234,
        }
    }

    #[test]
    fn decodes_opus_as_the_title_drives_it() {
        assert_eq!(size_of::<ApuResult>(), 0x50);
        // 256 mono/stereo streams, two commands each, as the title connects.
        let input = ConnectInput {
            process: DECODE_OPUS,
            streams: 256,
            commands_per_stream: 2,
            memory: 0x10_4000,
            frames_per_output: 0,
            options: 1,
        };
        let mut output = ConnectOutput {
            memory: std::ptr::null_mut(),
            result_event: std::ptr::null_mut(),
        };
        let mut handle = std::ptr::null_mut();
        assert_eq!(XApuConnect(&input, &mut output, &mut handle), S_OK);
        let base = output.memory.cast::<u8>();

        let activate = Activate {
            id: id(ACTIVATE, 3),
            channels: 2,
            init_length: 0,
            init: std::ptr::null(),
            start_pitch: 1.0,
            processing_length: 0,
            processing: std::ptr::null_mut(),
        };
        assert_eq!(
            XApuEnqueueCommand(handle, (&raw const activate).cast(), std::ptr::null_mut()),
            S_OK
        );

        // A 20 ms stereo packet of a 440 Hz tone.
        let tone: Vec<f32> = (0..960 * 2)
            .map(|i| ((i / 2) as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.5)
            .collect();
        let mut encoder = OpusEncoder::new(48_000, 2, Application::Audio).unwrap();
        let mut packet = [0u8; 1500];
        let n = encoder.encode(&tone, 960, &mut packet).unwrap();
        // SAFETY: inside the connection memory.
        unsafe { std::ptr::copy_nonoverlapping(packet.as_ptr(), base, n) };
        let process = Process {
            id: id(PROCESS, 3),
            pitch_ramp: 0.0,
            target_pitch: 1.0,
            first_frame: 0,
            frame_count: 960,
            input_length: n as u32,
            input: base,
            output_length: 0xf00 * 2,
            output: base.wrapping_add(0x14000),
        };
        let mut total = 0;
        assert_eq!(
            XApuEnqueueCommand(handle, (&raw const process).cast(), &mut total),
            S_OK
        );
        assert_eq!(total, 2);
        // Two results queued for stream 3: its queue is full.
        assert_eq!(
            XApuEnqueueCommand(handle, (&raw const process).cast(), &mut total),
            QUEUE_FULL
        );
        assert_eq!(XApuGetResultCount(handle, std::ptr::null_mut()), 2);

        let mut result = std::mem::MaybeUninit::<ApuResult>::zeroed();
        assert_eq!(XApuDequeueResult(handle, result.as_mut_ptr()), S_OK);
        // SAFETY: written above.
        let first = unsafe { result.assume_init() };
        let (kind, error, context) = (first.id.kind, first.error, first.id.context);
        assert_eq!((kind, error, context), (ACTIVATE, S_OK, 0x1234));
        assert_eq!(XApuDequeueResult(handle, result.as_mut_ptr()), S_OK);
        // SAFETY: written above.
        let decoded = unsafe { result.assume_init() };
        let (kind, error, frames, length) = (
            decoded.id.kind,
            decoded.error,
            decoded.frames,
            decoded.output_length,
        );
        assert_eq!((kind, error, frames, length), (PROCESS, S_OK, 960, 960 * 8));
        // SAFETY: the decoder wrote 960 stereo frames there.
        let pcm =
            unsafe { std::slice::from_raw_parts(base.wrapping_add(0x14000).cast::<f32>(), 1920) };
        let energy: f32 = pcm.iter().map(|s| s * s).sum::<f32>() / 1920.0;
        assert!(energy > 0.01, "decoded a tone, energy {energy}");
        assert_eq!(
            XApuDequeueResult(handle, result.as_mut_ptr()),
            PENDING_RESULTS
        );
        let mut dequeued = 0;
        assert_eq!(XApuGetResultCount(handle, &mut dequeued), 0);
        assert_eq!(dequeued, 2);

        // A process on a stream that was never activated is refused.
        let mut stray = process;
        stray.id.stream = 4;
        assert_eq!(
            XApuEnqueueCommand(handle, (&raw const stray).cast(), &mut total),
            INVALID_STREAM
        );
        // Activating twice is refused; the stream stays active until its
        // deactivation's result is dequeued.
        assert_eq!(
            XApuEnqueueCommand(handle, (&raw const activate).cast(), &mut total),
            ALREADY_ACTIVE
        );
        let deactivate = id(DEACTIVATE, 3);
        assert_eq!(XApuEnqueueCommand(handle, &deactivate, &mut total), S_OK);
        assert_eq!(total, 3);
        assert_eq!(
            XApuEnqueueCommand(handle, (&raw const activate).cast(), &mut total),
            ALREADY_ACTIVE
        );
        // Only the id, time and error of a non-process result are written.
        let mut record = [0xAAu8; 0x50];
        assert_eq!(XApuDequeueResult(handle, record.as_mut_ptr().cast()), S_OK);
        assert_eq!(record[0], DEACTIVATE as u8);
        assert_eq!(&record[0x18..0x24], &[0xAA; 12]);
        assert_eq!(&record[0x2c..0x30], &[0; 4]);
        assert_eq!(&record[0x30..], &[0xAA; 0x20]);
        assert_eq!(
            XApuEnqueueCommand(handle, (&raw const activate).cast(), &mut total),
            S_OK
        );
        // Its result is ready (commands complete at once): nothing pending.
        assert_eq!(XApuDisconnect(handle), S_OK);
    }

    #[test]
    fn decodes_into_unaligned_output_interleaved_and_deinterleaved() {
        let tone: Vec<f32> = (0..960 * 2)
            .map(|i| {
                let phase = (i / 2) as f32 * std::f32::consts::TAU / 48_000.0;
                if i % 2 == 0 {
                    (phase * 440.0).sin() * 0.5
                } else {
                    (phase * 880.0).sin() * 0.25
                }
            })
            .collect();
        let mut encoder = OpusEncoder::new(48_000, 2, Application::Audio).unwrap();
        let mut packet = [0u8; 1500];
        let n = encoder.encode(&tone, 960, &mut packet).unwrap();

        for (options, deinterleaved) in [(0u32, false), (DEINTERLEAVED_OUTPUT, true)] {
            let input = ConnectInput {
                process: DECODE_OPUS,
                streams: 4,
                commands_per_stream: 4,
                memory: 0x4_0000,
                frames_per_output: 0,
                options,
            };
            let mut output = ConnectOutput {
                memory: std::ptr::null_mut(),
                result_event: std::ptr::null_mut(),
            };
            let mut handle = std::ptr::null_mut();
            assert_eq!(XApuConnect(&input, &mut output, &mut handle), S_OK);
            let base = output.memory.cast::<u8>();

            // Invalid channel counts fail in the result; the stream is
            // active until deactivated.
            for bad_channels in [0u32, 3, u32::MAX] {
                let bad = Activate {
                    id: id(ACTIVATE, 0),
                    channels: bad_channels,
                    init_length: 0,
                    init: std::ptr::null(),
                    start_pitch: 1.0,
                    processing_length: 0,
                    processing: std::ptr::null_mut(),
                };
                assert_eq!(
                    XApuEnqueueCommand(handle, (&raw const bad).cast(), std::ptr::null_mut()),
                    S_OK
                );
                let mut res = std::mem::MaybeUninit::<ApuResult>::zeroed();
                assert_eq!(XApuDequeueResult(handle, res.as_mut_ptr()), S_OK);
                // SAFETY: written by `XApuDequeueResult`.
                let err = unsafe { res.assume_init() }.error;
                assert_eq!(err, E_INVALIDARG, "channels={bad_channels}");
                let deactivate = id(DEACTIVATE, 0);
                assert_eq!(
                    XApuEnqueueCommand(handle, &deactivate, std::ptr::null_mut()),
                    S_OK
                );
                assert_eq!(XApuDequeueResult(handle, res.as_mut_ptr()), S_OK);
            }

            // Activate two identical stereo decoders so aligned and unaligned outputs match bit-for-bit.
            for stream in [0u32, 1] {
                let activate = Activate {
                    id: id(ACTIVATE, stream),
                    channels: 2,
                    init_length: 0,
                    init: std::ptr::null(),
                    start_pitch: 1.0,
                    processing_length: 0,
                    processing: std::ptr::null_mut(),
                };
                assert_eq!(
                    XApuEnqueueCommand(handle, (&raw const activate).cast(), std::ptr::null_mut()),
                    S_OK
                );
                let mut res = std::mem::MaybeUninit::<ApuResult>::zeroed();
                assert_eq!(XApuDequeueResult(handle, res.as_mut_ptr()), S_OK);
                // SAFETY: written by `XApuDequeueResult`.
                let err = unsafe { res.assume_init() }.error;
                assert_eq!(err, S_OK);
            }

            // SAFETY: inside the connection memory.
            unsafe { std::ptr::copy_nonoverlapping(packet.as_ptr(), base, n) };
            let aligned_out = base.wrapping_add(0x10000);
            let unaligned_out = base.wrapping_add(0x20001); // 1 mod 4: misaligned for f32

            for (stream, out_ptr, frame_count) in [
                (0u32, aligned_out, 960u32),
                (1u32, unaligned_out, 960u32),
                (1u32, base.wrapping_add(0x30003), 0u32), // zero frames at misaligned ptr
            ] {
                let proc_cmd = Process {
                    id: id(PROCESS, stream),
                    pitch_ramp: 0.0,
                    target_pitch: 1.0,
                    first_frame: 0,
                    frame_count,
                    input_length: n as u32,
                    input: base,
                    output_length: 960 * 8,
                    output: out_ptr,
                };
                assert_eq!(
                    XApuEnqueueCommand(handle, (&raw const proc_cmd).cast(), std::ptr::null_mut()),
                    S_OK
                );
                let mut res = std::mem::MaybeUninit::<ApuResult>::zeroed();
                assert_eq!(XApuDequeueResult(handle, res.as_mut_ptr()), S_OK);
                // SAFETY: written by `XApuDequeueResult`.
                let decoded = unsafe { res.assume_init() };
                let (err, frames, len) = (decoded.error, decoded.frames, decoded.output_length);
                assert_eq!((err, frames, len), (S_OK, frame_count, frame_count * 8));
            }

            // SAFETY: both output regions are inside connection memory and hold 960 * 8 bytes.
            let (aligned_bytes, unaligned_bytes) = unsafe {
                (
                    std::slice::from_raw_parts(aligned_out, 960 * 8),
                    std::slice::from_raw_parts(unaligned_out, 960 * 8),
                )
            };
            assert_eq!(
                unaligned_bytes, aligned_bytes,
                "unaligned output must match aligned output (deinterleaved={deinterleaved})"
            );
            let samples: Vec<f32> = unaligned_bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|c| f32::from_le_bytes(*c))
                .collect();
            let energy: f32 = samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32;
            assert!(
                energy > 0.01,
                "expected decoded tone (deinterleaved={deinterleaved}), energy={energy}"
            );
            assert_eq!(XApuDisconnect(handle), S_OK);
        }
    }
}

#[cfg(test)]
mod console_checks {
    use super::*;

    #[test]
    fn connect_checks_as_the_console() {
        let mut input = ConnectInput {
            process: DECODE_OPUS,
            streams: 4,
            commands_per_stream: 2,
            memory: 0x1000,
            frames_per_output: 480,
            options: 1,
        };
        let mut output = ConnectOutput {
            memory: 1 as *mut c_void,
            result_event: std::ptr::null_mut(),
        };
        let mut handle = 1 as *mut Connection;
        assert_eq!(XApuConnect(&input, std::ptr::null_mut(), &mut handle), E_POINTER);
        assert!(handle.is_null());
        assert_eq!(XApuConnect(std::ptr::null(), &mut output, &mut handle), E_INVALIDARG);
        assert!(output.memory.is_null());
        // Frames per output mean nothing to plain decoding.
        assert_eq!(XApuConnect(&input, &mut output, &mut handle), S_OK);
        let mut use_ = Utilization {
            available: 0,
            total: 0,
        };
        assert_eq!(XApuGetUtilization(1, 1, &mut use_), S_OK);
        assert_eq!(use_.total, 500);
        assert!(use_.available <= 496);
        assert_eq!(XApuGetUtilization(4, 1, &mut use_), E_INVALIDARG);
        assert_eq!(XApuDisconnect(handle), S_OK);
        input.options = 0x10;
        assert_eq!(XApuConnect(&input, &mut output, &mut handle), E_INVALIDARG);
        input.options = 0;
        input.memory = 0x2000_0001;
        assert_eq!(XApuConnect(&input, &mut output, &mut handle), E_OUTOFMEMORY);
        input.memory = 0x1000;
        input.process = 2;
        input.frames_per_output = 0x81;
        assert_eq!(XApuConnect(&input, &mut output, &mut handle), E_INVALIDARG);
        input.streams = 501;
        assert_eq!(XApuConnect(&input, &mut output, &mut handle), E_INVALIDARG);
    }
}

#[cfg(test)]
mod bench {
    use super::*;
    use opus_pure::{Application, OpusEncoder};

    /// Time per 20 ms stereo packet through XApuEnqueueCommand and
    /// XApuDequeueResult. `cargo test --release -- --ignored bench
    /// --nocapture`.
    #[test]
    #[ignore]
    fn bench_opus_process() {
        let input = ConnectInput {
            process: DECODE_OPUS,
            streams: 1,
            commands_per_stream: 2,
            memory: 0x4_0000,
            frames_per_output: 0,
            options: 0,
        };
        let mut output = ConnectOutput {
            memory: std::ptr::null_mut(),
            result_event: std::ptr::null_mut(),
        };
        let mut handle = std::ptr::null_mut();
        assert_eq!(XApuConnect(&input, &mut output, &mut handle), S_OK);
        let base = output.memory.cast::<u8>();
        let id = |kind| CommandId {
            kind,
            stream: 0,
            sequence: 0,
            options: 0,
            context: 0,
        };
        let activate = Activate {
            id: id(ACTIVATE),
            channels: 2,
            init_length: 0,
            init: std::ptr::null(),
            start_pitch: 1.0,
            processing_length: 0,
            processing: std::ptr::null_mut(),
        };
        let mut result = std::mem::MaybeUninit::<ApuResult>::zeroed();
        assert_eq!(XApuEnqueueCommand(handle, (&raw const activate).cast(), std::ptr::null_mut()), S_OK);
        assert_eq!(XApuDequeueResult(handle, result.as_mut_ptr()), S_OK);
        let tone: Vec<f32> = (0..960 * 2).map(|i| ((i / 2) as f32 * 0.05).sin() * 0.4).collect();
        let mut encoder = OpusEncoder::new(48_000, 2, Application::Audio).unwrap();
        let mut packet = [0u8; 1500];
        let n = encoder.encode(&tone, 960, &mut packet).unwrap();
        // SAFETY: inside the connection memory.
        unsafe { std::ptr::copy_nonoverlapping(packet.as_ptr(), base, n) };
        let process = Process {
            id: id(PROCESS),
            pitch_ramp: 0.0,
            target_pitch: 1.0,
            first_frame: 0,
            frame_count: 960,
            input_length: n as u32,
            input: base,
            output_length: 960 * 8,
            output: base.wrapping_add(0x1_0000),
        };
        let packets = 2000;
        let start = std::time::Instant::now();
        for _ in 0..packets {
            assert_eq!(XApuEnqueueCommand(handle, (&raw const process).cast(), std::ptr::null_mut()), S_OK);
            assert_eq!(XApuDequeueResult(handle, result.as_mut_ptr()), S_OK);
        }
        let us = start.elapsed().as_secs_f64() * 1e6 / packets as f64;
        println!("bench opus: {us:.1} us per 20 ms stereo packet ({:.2}% of real time)", us / 20_000.0 * 100.0);
        assert_eq!(XApuDisconnect(handle), S_OK);
    }
}
