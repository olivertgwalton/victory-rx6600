//! How the console library talks to its storage driver: an
//! `SRB_IO_CONTROL` block (`MSFT_XVD`, a control code, a body) sent with
//! `IOCTL_SCSI_MINIPORT` on the adapter's handle, and the driver's status
//! mapped to an HRESULT. Addresses are `XCRDAPI.dll`'s.

use std::sync::atomic::{AtomicU32, Ordering};

use super::sys::{Handle, IoStatus, sys};
use super::{E_INVALIDARG, E_OUTOFMEMORY, HRESULT, hresult_from_win32};

/// `IOCTL_SCSI_MINIPORT`.
pub const IOCTL_SCSI_MINIPORT: u32 = 0x0004_d008;
/// `SRB_IO_CONTROL`'s size (`HeaderLength`).
pub const HEADER: usize = 0x1c;
/// "MSFT_XVD", the signature the driver answers to.
pub const SIGNATURE: [u8; 8] = *b"MSFT_XVD";
/// `STATUS_PENDING`.
const PENDING: i32 = 0x103;
/// `STATUS_DEVICE_BUSY`: tried again, 100 times 100 ms apart (0x18000e778).
const DEVICE_BUSY: i32 = 0x8000_0011_u32 as i32;
/// A body past this is refused before it is built ("Payload too large for
/// XVIO").
pub const MAX_BODY: usize = 0x10_0000;

/// An `SRB_IO_CONTROL` and its body (0x18000e7d0): header length 0x1c,
/// signature, the control code at +0x10, the body's length at +0x18, all
/// else 0.
pub struct Payload {
    pub bytes: Vec<u8>,
}

impl Payload {
    pub fn new(code: u32, body_len: usize) -> Payload {
        let mut bytes = vec![0; HEADER + body_len];
        bytes[0..4].copy_from_slice(&(HEADER as u32).to_le_bytes());
        bytes[4..12].copy_from_slice(&SIGNATURE);
        bytes[0x10..0x14].copy_from_slice(&code.to_le_bytes());
        bytes[0x18..0x1c].copy_from_slice(&(body_len as u32).to_le_bytes());
        Payload { bytes }
    }

    pub fn body(&mut self) -> &mut [u8] {
        &mut self.bytes[HEADER..]
    }

    pub fn put(&mut self, at: usize, bytes: &[u8]) {
        self.body()[at..at + bytes.len()].copy_from_slice(bytes);
    }

    pub fn u32(&mut self, at: usize, value: u32) {
        self.put(at, &value.to_le_bytes());
    }

    pub fn u64(&mut self, at: usize, value: u64) {
        self.put(at, &value.to_le_bytes());
    }

    pub fn read_u32(&self, at: usize) -> u32 {
        u32::from_le_bytes(self.bytes[HEADER + at..HEADER + at + 4].try_into().unwrap())
    }

    /// The driver's `ReturnCode` (+0x14), an NTSTATUS.
    fn return_code(&self) -> i32 {
        i32::from_le_bytes(self.bytes[0x14..0x18].try_into().unwrap())
    }
}

/// Where a failed send is described (the console's 0x30-byte record): the
/// source file's name (20 bytes), the source line, a value the caller
/// passed, and two zeros. Only the first failure is kept.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Failure {
    pub file: [u8; 20],
    pub line: u32,
    pub value: u64,
    pub zero: [u64; 2],
}

impl Failure {
    pub fn note(&mut self, line: u32, value: u64) {
        if self.line != 0 {
            return;
        }
        // strrchr(__FILE__, '\\') + 1 ("api.c"), at most 19 bytes and a NUL.
        let name = b"api.c";
        self.file = [0; 20];
        self.file[..name.len()].copy_from_slice(name);
        self.line = line;
        self.value = value;
        self.zero = [0; 2];
    }
}

/// 0x18000e50c: sends and keeps no failure record.
pub fn send(handle: Handle, payload: &mut Payload) -> HRESULT {
    send_noting(handle, payload, None, &mut Failure::default())
}

/// 0x18000e55c. Its value slot (set to 0xa first, copied into the failure
/// record) is a local, or the body's bytes at `value_at` when the caller
/// points it there.
pub fn send_noting(handle: Handle, payload: &mut Payload, value_at: Option<usize>, failure: &mut Failure) -> HRESULT {
    let local = 0xa_u64;
    if let Some(at) = value_at {
        payload.u64(at, 0xa);
    }
    let value = |p: &Payload, local: u64| match value_at {
        Some(at) => u64::from_le_bytes(p.bytes[HEADER + at..HEADER + at + 8].try_into().unwrap()),
        None => local,
    };
    let s = sys();
    // SAFETY: a plain auto-reset event.
    let event = unsafe { (s.create_event_w)(std::ptr::null(), 0, 0, std::ptr::null()) };
    if event.is_null() {
        failure.note(0x1a0b, value(payload, local));
        return hresult_from_win32(super::sys::last_error());
    }
    let len = payload.bytes.len() as u32;
    let mut tries = 0;
    let status = loop {
        let mut io = IoStatus::default();
        let ptr = payload.bytes.as_mut_ptr().cast();
        // SAFETY: the payload is ours and outlives the (waited-for) call.
        let mut status = unsafe {
            (s.nt_device_io_control_file)(handle, event, std::ptr::null(), std::ptr::null(), &mut io, IOCTL_SCSI_MINIPORT, ptr, len, ptr, len)
        };
        if status == PENDING {
            // SAFETY: our event.
            let waited = unsafe { (s.nt_wait_for_single_object)(event, 0, std::ptr::null()) };
            status = if waited >= 0 { io.status as i32 } else { waited };
        }
        let code = payload.return_code();
        if status >= 0 {
            status = code;
        } else if code < 0 {
            status = code;
            failure.note(0x1a28, value(payload, local));
        } else {
            failure.note(0x1a2a, value(payload, local));
        }
        tries += 1;
        if status != DEVICE_BUSY || tries >= 100 {
            break status;
        }
        // SAFETY: plain.
        unsafe { (s.sleep)(100) };
    };
    // SAFETY: our event.
    unsafe { (s.close_handle)(event) };
    hresult(status)
}

/// 0x18000c814: an NTSTATUS as the library's HRESULT. 0 is S_OK; a
/// customer status (bit 29) loses that bit; the driver's own statuses
/// (0xC05E00xx) are facility 0x924; others are the matching Win32 error,
/// or `HRESULT_FROM_NT` when there is none (ERROR_MR_MID_NOT_FOUND).
pub fn hresult(status: i32) -> HRESULT {
    let s = status as u32;
    if s == 0 {
        return 0;
    }
    if s & (1 << 29) != 0 {
        return (s & !(1 << 29)) as i32;
    }
    if s.wrapping_add(0x3fa2_0000) <= 0xff {
        return (s & 0xffff | 0x8924_0000) as i32;
    }
    match super::sys::dos_error(status) {
        0x13d => (s | 1 << 28) as i32,
        0 => 0,
        e if (e as i32) < 0 => e as i32,
        e => hresult_from_win32(e),
    }
}

/// The trace flags (`XCrdSetTraceFlags`, 0x18001b688): messages whose
/// class (bit 0 errors, 1 and 2 more) is set are printed.
pub static TRACE: AtomicU32 = AtomicU32::new(0);

/// 0x180001b1c: a message, printed when its class is on (formatted into
/// 0x1ff characters, as the console's).
pub fn log(class: u32, message: &str) {
    if TRACE.load(Ordering::Relaxed) & class == 0 {
        return;
    }
    let mut wide: Vec<u16> = message.encode_utf16().take(0x1ff).collect();
    wide.push(0);
    // SAFETY: a NUL-terminated line.
    unsafe { (sys().putws)(wide.as_ptr()) };
}

/// Checks an argument and logs its message (class 1) when it fails.
pub fn check(ok: bool, message: &str) -> Result<(), HRESULT> {
    if ok {
        Ok(())
    } else {
        log(1, message);
        Err(E_INVALIDARG)
    }
}

/// 0x180001ba0 (`StringCchCopyW` into a body): copies `source` into
/// `capacity` characters at `at`; 0 when it fits, `0x80000005`
/// (STATUS_BUFFER_OVERFLOW) truncated, `0xC000000D` for a capacity of 0 or
/// past `0x7ffffffe`.
pub fn copy_string(body: &mut [u8], at: usize, capacity_bytes: usize, source: &[u16]) -> i32 {
    let capacity = capacity_bytes / 2;
    if capacity == 0 || capacity - 1 > 0x7fff_fffe {
        if capacity != 0 {
            body[at..at + 2].fill(0);
        }
        return 0xc000_000d_u32 as i32;
    }
    let n = source.iter().take_while(|&&c| c != 0).count();
    let copied = n.min(capacity - 1);
    for (i, c) in source[..copied].iter().enumerate() {
        body[at + 2 * i..at + 2 * i + 2].copy_from_slice(&c.to_le_bytes());
    }
    body[at + 2 * copied..at + 2 * copied + 2].fill(0);
    if copied < n { 0x8000_0005_u32 as i32 } else { 0 }
}

pub fn alloc_failed() -> HRESULT {
    E_OUTOFMEMORY
}
