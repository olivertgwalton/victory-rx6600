//! XVD transfers: the library serves a named pipe (`\\.\HSVPipe\XvdXferPipe`)
//! that the host's transfer service connects to. Messages are 0x210 bytes,
//! `{type, 0, status or id, ...}`: start (1, answered by 2 with the
//! transfer id), read (3 `{id, slot, size, offset}`, answered by 4 `{status,
//! id, slot, bytes}` and the data), stop (5, answered by 6; reads still in
//! flight answer 4 first). Reads complete on a thread of the transfer's own,
//! through the caller's OVERLAPPED. Addresses are `XCRDAPI.dll`'s.

use std::sync::Mutex;
use std::sync::atomic::{AtomicI32, AtomicU8, Ordering};

use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::Threading::{INFINITE, ResetEvent, SetEvent, WaitForMultipleObjects, WaitForSingleObject};

use super::srb::{Payload, log, send};
use super::sys::{self, Handle, sys};
use super::{Adapter, E_INVALIDARG, HRESULT, handle_of, hresult_from_win32, wide};

const MESSAGE: usize = 0x210;
const NOT_READY: HRESULT = 0x8007_0015_u32 as i32;
const BAD_COMMAND: HRESULT = 0x8007_0016_u32 as i32;
const INVALID: Handle = -1isize as Handle;

/// The caller's OVERLAPPED: status, bytes, offset, event.
#[repr(C)]
pub struct Overlapped {
    pub internal: u64,
    pub internal_high: u64,
    pub offset: u64,
    pub event: Handle,
}

#[derive(Clone, Copy)]
struct Slot {
    busy: bool,
    overlapped: *mut Overlapped,
    buffer: *mut u8,
    size: u32,
}

const FREE: Slot = Slot { busy: false, overlapped: std::ptr::null_mut(), buffer: std::ptr::null_mut(), size: 0 };

/// A transfer session (the library's 0x260-byte block).
pub struct Xfer {
    pipe: Handle,
    session: u32,
    transfer: u32,
    thread: Option<std::thread::JoinHandle<()>>,
    teardown: Handle,
    /// Torn down (no reads), and why.
    down: AtomicU8,
    error: AtomicI32,
    slots: Option<Mutex<[Slot; 16]>>,
    slot_freed: Handle,
}

// SAFETY: the raw pointers are the caller's buffers and Win32 handles, used
// as the library uses them from its completion thread.
unsafe impl Send for Xfer {}
unsafe impl Sync for Xfer {}

struct Shared(*mut Xfer);
// SAFETY: as above; the session outlives its thread (Stop joins it).
unsafe impl Send for Shared {}

fn last_hresult() -> HRESULT {
    hresult_from_win32(sys::last_error())
}

impl Xfer {
    /// Marks the session torn down with this error, unless it already is.
    fn fail(&self, error: HRESULT) {
        if self.down.load(Ordering::SeqCst) != 1 {
            self.down.store(1, Ordering::SeqCst);
            self.error.store(error, Ordering::SeqCst);
        }
    }

    /// 0x18000d028.
    fn release(&self, slot: usize) {
        if let Some(slots) = &self.slots {
            slots.lock().unwrap_or_else(|e| e.into_inner())[slot] = FREE;
        }
        // SAFETY: our event.
        unsafe { SetEvent(self.slot_freed) };
    }
}

fn event(manual: bool) -> Handle {
    // SAFETY: a plain unnamed event.
    unsafe { (sys().create_event_w)(std::ptr::null(), i32::from(manual), 0, std::ptr::null()) }
}

/// The library prints these narrow names through a wide `%s`: pairs of
/// bytes as characters, to the first zero pair.
fn as_wide(narrow: &[u8]) -> String {
    let units: Vec<u16> = narrow.chunks(2).map(|c| u16::from_le_bytes([c[0], c.get(1).copied().unwrap_or(0)])).take_while(|&c| c != 0).collect();
    String::from_utf16_lossy(&units)
}

/// 0x18000cd68: all of `len` bytes written to or read from the pipe,
/// overlapped, giving up when the session is torn down.
fn pipe_io(x: &Xfer, write: bool, mut buffer: *mut u8, mut len: u32) -> HRESULT {
    let s = sys();
    let done = event(false);
    if done.is_null() {
        return last_hresult();
    }
    let waits = [done, x.teardown];
    let mut overlapped = Overlapped { internal: 0, internal_high: 0, offset: 0, event: done };
    let (doing, did, way) = if write { ("Writing", "Wrote", "to") } else { ("Reading", "Read", "from") };
    let mut hr = 0;
    while len != 0 {
        log(4, &format!("XCRD-XFER: {doing} {len} bytes {way} pipe"));
        let o = (&mut overlapped as *mut Overlapped).cast();
        // SAFETY: our pipe, the caller's buffer of `len` bytes, our OVERLAPPED.
        let started = unsafe {
            if write { (s.write_file)(x.pipe, buffer.cast(), len, std::ptr::null_mut(), o) } else { (s.read_file)(x.pipe, buffer.cast(), len, std::ptr::null_mut(), o) }
        };
        if started == 0 {
            if sys::last_error() != 0x3e5 {
                hr = last_hresult();
                log(1, &format!("XCRD-XFER: {} failed 0x{:08X}", if write { "WriteFile" } else { "ReadFile" }, hr as u32));
                break;
            }
            // SAFETY: our two events.
            let which = unsafe { WaitForMultipleObjects(2, waits.as_ptr().cast(), 0, INFINITE) };
            if which > 1 {
                hr = last_hresult();
                log(1, &format!("XCRD-XFER: Pipe wait error 0x{:08X} ({which})", hr as u32));
                break;
            }
            if waits[which as usize] == x.teardown {
                let mut n = 0;
                // SAFETY: our pipe and OVERLAPPED.
                unsafe {
                    (s.cancel_io_ex)(x.pipe, o);
                    (s.get_overlapped_result)(x.pipe, o, &mut n, 1);
                }
                log(2, &format!("XCRD-XFER: Pipe {} aborted due to teardown", if write { "write" } else { "read" }));
                hr = 0x8007_03e3_u32 as i32;
                x.fail(hr);
                break;
            }
        }
        let mut n = 0;
        // SAFETY: as above.
        if unsafe { (s.get_overlapped_result)(x.pipe, o, &mut n, 0) } == 0 {
            hr = last_hresult();
            log(1, &format!("XCRD-XFER: Error getting pipe {} result 0x{:08X}", if write { as_wide(b"write\0\0\0read") } else { as_wide(b"read\0\0\0\0") }, hr as u32));
            break;
        }
        log(4, &format!("XCRD-XFER: {did} {n} bytes {way} pipe"));
        // SAFETY: within the caller's buffer.
        buffer = unsafe { buffer.add(n as usize) };
        len -= n;
    }
    // SAFETY: our event.
    unsafe { (s.close_handle)(done) };
    hr
}

fn message(kind: u32) -> [u8; MESSAGE] {
    let mut m = [0u8; MESSAGE];
    m[0..4].copy_from_slice(&kind.to_le_bytes());
    m
}

fn field(m: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(m[at..at + 4].try_into().unwrap())
}

fn put(m: &mut [u8], at: usize, v: u32) {
    m[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

/// 0x18000d080: completes reads as their answers come.
fn completions(x: &Xfer) {
    let mut m = [0u8; MESSAGE];
    let mut hr = pipe_io(x, false, m.as_mut_ptr(), MESSAGE as u32);
    let mut aborted = false;
    while hr >= 0 {
        let slot = field(&m, 0x10) as usize;
        if field(&m, 0) != 4 || field(&m, 0xc) != x.transfer || slot >= 16 {
            hr = BAD_COMMAND;
            break;
        }
        let status = super::srb::hresult(field(&m, 8) as i32);
        let bytes = field(&m, 0x14);
        log(4, &format!("XCRD-XFER: Read ack status 0x{:08X}, bytes read {bytes}", status as u32));
        let s = x.slots.as_ref().map(|s| s.lock().unwrap_or_else(|e| e.into_inner())[slot]).unwrap_or(FREE);
        // SAFETY: the caller's OVERLAPPED of this read.
        let o = unsafe { &mut *s.overlapped };
        o.internal = status as i64 as u64;
        o.internal_high = u64::from(bytes);
        hr = 0;
        if status >= 0 && bytes != 0 {
            let r = pipe_io(x, false, s.buffer, bytes);
            hr = r;
            if r < 0 {
                x.fail(r);
                o.internal = r as i64 as u64;
                o.internal_high = 0;
            }
        }
        // SAFETY: the caller's event.
        unsafe { SetEvent(o.event) };
        x.release(slot);
        if hr < 0 {
            aborted = true;
            break;
        }
        m = [0u8; MESSAGE];
        hr = pipe_io(x, false, m.as_mut_ptr(), MESSAGE as u32);
    }
    if !aborted {
        x.fail(hr);
    }
    if let Some(slots) = &x.slots {
        let guard = slots.lock().unwrap_or_else(|e| e.into_inner());
        let busy: Vec<(usize, Slot)> = guard.iter().copied().enumerate().filter(|(_, s)| s.busy).collect();
        drop(guard);
        for (i, s) in busy {
            // SAFETY: the caller's OVERLAPPED.
            let o = unsafe { &mut *s.overlapped };
            o.internal = x.error.load(Ordering::SeqCst) as i64 as u64;
            o.internal_high = 0;
            // SAFETY: its event.
            unsafe { SetEvent(o.event) };
            x.release(i);
        }
    }
    log(4, &format!("XCRD-XFER: Completion thread exiting 0x{:08X}", hr as u32));
}

/// 0x18000bb60: waits for a read and gives its status and byte count.
pub extern "system" fn XCrdXvdXferGetResult(_xfer: *mut Xfer, overlapped: *const Overlapped, bytes: *mut u32) -> HRESULT {
    // SAFETY: the caller's OVERLAPPED (not checked) and out.
    unsafe {
        WaitForSingleObject((*overlapped).event, INFINITE);
        *bytes = (*overlapped).internal_high as u32;
        (*overlapped).internal as u32 as i32
    }
}

/// 0x18000bb90: a session (control 0xc0, `{_, 1}`) and the pipe, connected
/// within five seconds.
pub extern "system" fn XCrdXvdXferInitialize(adapter: *const Adapter, out: *mut *mut Xfer) -> HRESULT {
    if adapter.is_null() {
        log(1, "XCRD-XFER: Invalid adapter handle");
        return E_INVALIDARG;
    }
    if out.is_null() {
        log(1, "XCRD-XFER: Invalid transfer handle");
        return E_INVALIDARG;
    }
    // SAFETY: the caller's out.
    unsafe { *out = std::ptr::null_mut() };
    log(4, "XCRD-XFER: Initializing");
    let x = Box::into_raw(Box::new(Xfer {
        pipe: INVALID,
        session: u32::MAX,
        transfer: u32::MAX,
        thread: None,
        teardown: std::ptr::null_mut(),
        down: AtomicU8::new(1),
        error: AtomicI32::new(NOT_READY),
        slots: None,
        slot_freed: std::ptr::null_mut(),
    }));
    // SAFETY: ours.
    let xr = unsafe { &mut *x };
    let hr = (|| {
        xr.teardown = event(true);
        if xr.teardown.is_null() {
            return last_hresult();
        }
        let name: Vec<u16> = "\\\\.\\HSVPipe\\XvdXferPipe".encode_utf16().chain([0]).collect();
        // SAFETY: a NUL-terminated name; duplex, overlapped, local clients
        // only, one instance.
        xr.pipe = unsafe { (sys().create_named_pipe_w)(name.as_ptr(), 0x4000_0003, 8, 1, 0, 0, 0, std::ptr::null()) };
        if xr.pipe == INVALID {
            return last_hresult();
        }
        let mut p = Payload::new(0x7800_00c0, 0x20);
        p.body()[4] = 1;
        let hr = send(handle_of(adapter), &mut p);
        if hr < 0 {
            return hr;
        }
        xr.session = p.read_u32(0);
        log(4, &format!("XCRD-XFER: Session ID is {}", xr.session));
        let connected = event(true);
        if connected.is_null() {
            return last_hresult();
        }
        let mut overlapped = Overlapped { internal: 0, internal_high: 0, offset: 0, event: connected };
        // SAFETY: our pipe and OVERLAPPED (its event is never closed, as
        // the library's).
        if unsafe { (sys().connect_named_pipe)(xr.pipe, (&mut overlapped as *mut Overlapped).cast()) } != 0 {
            return 0x8000_ffff_u32 as i32;
        }
        let e = last_hresult();
        if e == 0x8007_0217_u32 as i32 {
            // SAFETY: ours.
            unsafe { SetEvent(connected) };
        } else if e != 0x8007_03e5_u32 as i32 {
            return e;
        }
        // SAFETY: ours.
        match unsafe { WaitForSingleObject(connected, 5000) } {
            WAIT_OBJECT_0 => {
                // SAFETY: the caller's out.
                unsafe { *out = x };
                0
            }
            WAIT_TIMEOUT => 0x8007_05b4_u32 as i32,
            _ => last_hresult(),
        }
    })();
    // SAFETY: the caller's out.
    if unsafe { (*out).is_null() } {
        let mut gone = x;
        XCrdXvdXferUninitialize(adapter, &mut gone);
    }
    log(4, &format!("XCRD-XFER: Initialize result 0x{:X}", hr as u32));
    hr
}

/// 0x18000c000: starts the transfer of a CRD (message 1, answered by 2) and
/// its completion thread.
pub extern "system" fn XCrdXvdXferStart(x: *mut Xfer, path: *const u16) -> HRESULT {
    if x.is_null() {
        log(1, "XCRD-XFER: Invalid transfer handle");
        return E_INVALIDARG;
    }
    if path.is_null() {
        log(1, "XCRD-XFER: Invalid CRD path");
        return E_INVALIDARG;
    }
    // SAFETY: a session of ours.
    let xr = unsafe { &mut *x };
    if xr.pipe == INVALID || xr.session == u32::MAX || xr.transfer != u32::MAX {
        log(1, "XCRD-XFER: Invalid transfer context");
        return 0x8007_139f_u32 as i32;
    }
    // SAFETY: a NUL-terminated path.
    let path = unsafe { wide(path) };
    log(4, &format!("XCRD-XFER: Starting transfer for {}", String::from_utf16_lossy(path)));
    let hr = (|| {
        let mut m = message(1);
        // wcscpy_s into 0x104 characters (a longer path is the CRT's fatal
        // error there).
        for (i, c) in path.iter().take(0x103).enumerate() {
            m[8 + 2 * i..10 + 2 * i].copy_from_slice(&c.to_le_bytes());
        }
        let hr = pipe_io(xr, true, m.as_mut_ptr(), MESSAGE as u32);
        if hr < 0 {
            return Err(hr);
        }
        let mut m = [0u8; MESSAGE];
        let hr = pipe_io(xr, false, m.as_mut_ptr(), MESSAGE as u32);
        if hr < 0 {
            return Err(hr);
        }
        if field(&m, 0) != 2 {
            return Err(BAD_COMMAND);
        }
        let hr = super::srb::hresult(field(&m, 8) as i32);
        if hr < 0 {
            return Err(hr);
        }
        xr.transfer = field(&m, 0xc);
        log(4, &format!("XCRD-XFER: Transfer ID is {}", xr.transfer));
        xr.slots = Some(Mutex::new([FREE; 16]));
        xr.slot_freed = event(false);
        if xr.slot_freed.is_null() {
            let hr = last_hresult();
            return if hr < 0 { Err(hr) } else { Ok(hr) };
        }
        let shared = Shared(x);
        xr.thread = Some(std::thread::spawn(move || {
            let shared = shared;
            // SAFETY: the session outlives its thread.
            completions(unsafe { &*shared.0 });
        }));
        if xr.down.load(Ordering::SeqCst) != 0 {
            xr.error.store(0, Ordering::SeqCst);
            xr.down.store(0, Ordering::SeqCst);
        }
        Ok(0)
    })();
    let hr = match hr {
        Ok(hr) => hr,
        Err(hr) => {
            XCrdXvdXferStop(x);
            hr
        }
    };
    log(4, &format!("XCRD-XFER: Start result 0x{:X}", hr as u32));
    hr
}

/// 0x18000be60: asks for `size` bytes at `offset` into `buffer` (message
/// 3 in a free slot of 16, waiting for one); ERROR_IO_PENDING, the
/// completion coming through the OVERLAPPED.
pub extern "system" fn XCrdXvdXferRead(x: *mut Xfer, offset: u64, size: u32, buffer: *mut u8, overlapped: *mut Overlapped) -> HRESULT {
    if x.is_null() {
        log(1, "XCRD-XFER: Invalid transfer handle");
        return E_INVALIDARG;
    }
    if buffer.is_null() {
        log(1, "XCRD-XFER: Invalid buffer");
        return E_INVALIDARG;
    }
    if overlapped.is_null() {
        log(1, "XCRD-XFER: Invalid overlapped");
        return E_INVALIDARG;
    }
    log(4, &format!("XCRD-XFER: Reading 0x{size:X} bytes from offset 0x{offset:X}"));
    // SAFETY: a session of ours.
    let xr = unsafe { &*x };
    if xr.down.load(Ordering::SeqCst) != 0 {
        return xr.error.load(Ordering::SeqCst);
    }
    let mut m = message(3);
    put(&mut m, 8, xr.transfer);
    m[0x18..0x20].copy_from_slice(&offset.to_le_bytes());
    put(&mut m, 0x10, size);
    let Some(slots) = &xr.slots else {
        return xr.error.load(Ordering::SeqCst);
    };
    let slot = loop {
        let mut guard = slots.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(i) = guard.iter().position(|s| !s.busy) {
            guard[i].busy = true;
            put(&mut m, 0xc, i as u32);
            break i;
        }
        drop(guard);
        // SAFETY: our event.
        unsafe { WaitForSingleObject(xr.slot_freed, INFINITE) };
    };
    if xr.down.load(Ordering::SeqCst) != 0 {
        xr.release(slot);
        return xr.error.load(Ordering::SeqCst);
    }
    {
        let mut guard = slots.lock().unwrap_or_else(|e| e.into_inner());
        guard[slot] = Slot { busy: true, overlapped, buffer, size };
    }
    let hr = pipe_io(xr, true, m.as_mut_ptr(), MESSAGE as u32);
    if hr >= 0 {
        return 0x8007_03e5_u32 as i32;
    }
    xr.release(slot);
    hr
}

/// 0x18000c230: tears the session down (its thread, slots), then stops the
/// transfer (message 5; reads still answering, then 6).
pub extern "system" fn XCrdXvdXferStop(x: *mut Xfer) -> HRESULT {
    if x.is_null() {
        log(1, "XCRD-XFER: Invalid transfer handle");
        return E_INVALIDARG;
    }
    // SAFETY: a session of ours.
    let xr = unsafe { &mut *x };
    log(4, &format!("XCRD-XFER: Stopping transfer {}", xr.transfer));
    xr.fail(NOT_READY);
    if let Some(thread) = xr.thread.take() {
        // SAFETY: our event.
        unsafe { SetEvent(xr.teardown) };
        let _ = thread.join();
        // SAFETY: as above.
        unsafe { ResetEvent(xr.teardown) };
    }
    if !xr.slot_freed.is_null() {
        // SAFETY: ours.
        unsafe { (sys().close_handle)(xr.slot_freed) };
        xr.slot_freed = std::ptr::null_mut();
    }
    xr.slots = None;
    let hr = (|| {
        let id = xr.transfer;
        if id == u32::MAX {
            return 0;
        }
        let mut m = message(5);
        put(&mut m, 8, id);
        let hr = pipe_io(xr, true, m.as_mut_ptr(), MESSAGE as u32);
        if hr < 0 {
            return hr;
        }
        loop {
            m = [0u8; MESSAGE];
            let hr = pipe_io(xr, false, m.as_mut_ptr(), MESSAGE as u32);
            if hr < 0 {
                return hr;
            }
            if field(&m, 0) != 4 {
                break;
            }
        }
        if field(&m, 0) != 6 {
            return BAD_COMMAND;
        }
        let hr = super::srb::hresult(field(&m, 8) as i32);
        if hr < 0 {
            return hr;
        }
        xr.transfer = u32::MAX;
        0
    })();
    log(4, &format!("XCRD-XFER: Stop result 0x{:X}", hr as u32));
    hr
}

/// 0x18000c3f0: stops, ends the session (control 0xc0, `{id, 0}`), closes
/// the pipe and frees the session.
pub extern "system" fn XCrdXvdXferUninitialize(adapter: *const Adapter, x: *mut *mut Xfer) -> HRESULT {
    if adapter.is_null() {
        log(1, "XCRD-XFER: Invalid adapter handle");
        return E_INVALIDARG;
    }
    // SAFETY: the caller's.
    if x.is_null() || unsafe { (*x).is_null() } {
        log(1, "XCRD-XFER: Invalid transfer handle");
        return E_INVALIDARG;
    }
    // SAFETY: as above.
    let session = unsafe { *x };
    // SAFETY: a session of ours.
    let xr = unsafe { &mut *session };
    log(4, &format!("XCRD-XFER: Uninitializing transfer session {}", xr.session));
    let hr = (|| {
        let hr = XCrdXvdXferStop(session);
        if hr < 0 {
            return hr;
        }
        if xr.session != u32::MAX {
            let mut p = Payload::new(0x7800_00c0, 0x20);
            p.u32(0, xr.session);
            let hr = send(handle_of(adapter), &mut p);
            if hr < 0 {
                return hr;
            }
        }
        let s = sys();
        if xr.pipe != INVALID {
            // SAFETY: our pipe.
            unsafe {
                (s.disconnect_named_pipe)(xr.pipe);
                (s.close_handle)(xr.pipe);
            }
            xr.pipe = INVALID;
        }
        if !xr.teardown.is_null() {
            // SAFETY: ours.
            unsafe { (s.close_handle)(xr.teardown) };
            xr.teardown = std::ptr::null_mut();
        }
        // SAFETY: made by Box::into_raw; the caller's.
        unsafe {
            drop(Box::from_raw(session));
            *x = std::ptr::null_mut();
        }
        0
    })();
    log(4, &format!("XCRD-XFER: Uninitialization result 0x{:X}", hr as u32));
    hr
}

pub(crate) fn exports() -> Vec<crate::HleExport> {
    use crate::hle_export;
    vec![
        hle_export!(XCrdXvdXferGetResult),
        hle_export!(XCrdXvdXferInitialize),
        hle_export!(XCrdXvdXferRead),
        hle_export!(XCrdXvdXferStart),
        hle_export!(XCrdXvdXferStop),
        hle_export!(XCrdXvdXferUninitialize),
    ]
}
