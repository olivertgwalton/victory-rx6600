//! `xgameruntime.dll`: the console game runtime (XGame*, XUser*, XTaskQueue*...).
//!
//! Titles rarely import it. A stub linked into the title loads the DLL with
//! `LoadLibraryExA` (answered with a pseudo handle, see `kernel`), looks up
//! the `*ApiImpl` functions and `XErrorReport` with `GetProcAddress`, and
//! initialises. Each runtime API call then asks `QueryApiImpl` for a COM-style
//! object of its area (provider and interface GUIDs), calls one method and
//! releases it. Until an area is implemented its query fails, and the call
//! returns that error without touching the object. An area's interface id
//! changes when its method table does, so each object answers only the ids
//! whose slots we know. Implemented, as an offline retail console with one
//! signed-in user: task queues and async blocks (`xasync`), the user
//! (`xuser`), system information (`xsystem`), title id, feature checks,
//! invites and events (`xgame`), persistent local storage (`storage`), game
//! saves (`xgamesave`), networking (`xnetworking`), the package
//! (`xpackage`), the store (`xstore`), system dialogs (`xgameui`), speech
//! (`speech`), HDR, URIs, display deferrals and screenshots (`display`).
#![allow(non_snake_case)]
// Title code calls these through its import table, passing pointers the real
// API would trust.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::{CStr, c_char, c_void};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use super::{
    display, speech, storage, xasync, xgame, xgamesave, xgameui, xnetworking, xpackage, xstore,
    xsystem, xuser,
};
use crate::{HleModule, hle_export};

pub type HRESULT = i32;
pub const S_OK: HRESULT = 0;
/// `HRESULT_FROM_WIN32(ERROR_NOT_SUPPORTED)`.
pub const NOT_SUPPORTED: HRESULT = 0x8007_0032_u32 as i32;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Guid {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

impl Guid {
    pub const fn new(data1: u32, data2: u16, data3: u16, data4: [u8; 8]) -> Guid {
        Guid {
            data1,
            data2,
            data3,
            data4,
        }
    }
}

impl std::fmt::Display for Guid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let d = self.data4;
        write!(
            f,
            "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            self.data1, self.data2, self.data3, d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]
        )
    }
}

/// A runtime object as title code calls it: its first word points at the
/// method table. Ours are static, so reference counting is a no-op.
#[repr(C)]
pub struct Object {
    vtable: *const *const (),
    /// Named in the log when a title calls a method we lack.
    area: &'static str,
}

// SAFETY: the table is a static array of function addresses.
unsafe impl Sync for Object {}

impl Object {
    pub const fn new<const N: usize>(area: &'static str, methods: &'static Methods<N>) -> Object {
        Object {
            vtable: methods.0.as_ptr(),
            area,
        }
    }
}

/// An object's method table, by slot (`QueryInterface`, `AddRef`,
/// `Release`, then the area's methods from offset 0x18).
pub struct Methods<const N: usize>(pub [*const (); N]);

// SAFETY: function addresses only.
unsafe impl<const N: usize> Sync for Methods<N> {}

/// A (provider, interface id) pair a title may ask for, and our object.
pub type Interface = (Guid, Guid, &'static Object);

const AREAS: &[&[Interface]] = &[
    xasync::INTERFACES,
    xuser::INTERFACES,
    xsystem::INTERFACES,
    xgame::INTERFACES,
    storage::INTERFACES,
    xnetworking::INTERFACES,
    xpackage::INTERFACES,
    display::INTERFACES,
    xgamesave::INTERFACES,
    xstore::INTERFACES,
    xgameui::INTERFACES,
    speech::INTERFACES,
];

/// `AddRef` and `Release`.
pub extern "system" fn add_ref(_: *const Object) -> u32 {
    1
}

/// A slot nobody has needed yet; logged with its area. Wrong for a slot
/// that returns `bool` or a value.
pub extern "system" fn missing<const SLOT: usize>(this: *const Object) -> HRESULT {
    // SAFETY: title code calls methods on objects we handed out.
    let area = unsafe { (*this).area };
    tracing::warn!(
        area,
        slot = format_args!("{:#x}", SLOT * 8),
        "runtime method not implemented"
    );
    xasync::E_NOTIMPL
}

/// Writes `value` through `out` if it is non-null.
pub fn put<T>(out: *mut T, value: T) -> HRESULT {
    if out.is_null() {
        return xasync::E_INVALIDARG;
    }
    // SAFETY: checked non-null; the title passes room for a `T`.
    unsafe { out.write(value) };
    S_OK
}

/// Copies `text` and a NUL into the title's `size`-byte buffer and reports
/// the bytes used, as the runtime's string getters do.
pub fn put_string(text: &str, size: usize, buffer: *mut u8, used: *mut usize) -> HRESULT {
    let len = text.len() + 1;
    if buffer.is_null() || size < len {
        return xasync::E_NOT_SUFFICIENT_BUFFER;
    }
    // SAFETY: the buffer holds `size` bytes, checked above.
    unsafe {
        buffer.copy_from_nonoverlapping(text.as_ptr(), text.len());
        buffer.add(text.len()).write(0);
    }
    if !used.is_null() {
        // SAFETY: checked non-null.
        unsafe { used.write(len) };
    }
    S_OK
}

/// The title's NUL-terminated string, if any.
pub fn text(s: *const c_char) -> Option<String> {
    // SAFETY: the title passes a NUL-terminated string or null.
    (!s.is_null()).then(|| unsafe { CStr::from_ptr(s) }.to_string_lossy().into_owned())
}

/// A new registration token for callbacks we never call (events nobody
/// raises offline).
pub fn token() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Calls method `slot` (byte offset) of `object` as title code would.
#[cfg(test)]
pub(crate) fn method<F: Copy>(object: &Object, slot: usize) -> F {
    // SAFETY: the caller names the slot's signature; tables cover the slots
    // tests use.
    unsafe { std::mem::transmute_copy(&*object.vtable.add(slot / 8)) }
}

pub extern "system" fn XGameRuntimeInitialize() -> HRESULT {
    tracing::info!("XGameRuntimeInitialize");
    S_OK
}

pub extern "system" fn XGameRuntimeUninitialize() {
    tracing::info!("XGameRuntimeUninitialize");
}

/// `E_GAMERUNTIME_NOT_INITIALIZED`: a module asked to join an
/// initialisation that has not happened.
pub const NOT_INITIALIZED: HRESULT = 0x8924_0100_u32 as i32;
/// `E_GAMERUNTIME_VERSION_MISMATCH`: the title was built for a newer runtime.
pub const VERSION_MISMATCH: HRESULT = 0x8924_0102_u32 as i32;
const E_NOINTERFACE: HRESULT = 0x8000_4002_u32 as i32;
/// IUnknown, which every area's object also answers.
const IUNKNOWN: Guid = Guid::new(0, 0, 0, [0xc0, 0, 0, 0, 0, 0, 0, 0x46]);

/// The console's xgameruntime.dll is this runtime version (major, minor);
/// titles built for a later one are refused.
const RUNTIME_VERSION: (u64, u64) = (0x6336, 0x2712);

/// Initialisation flags: bit 0 joins an existing initialisation (a DLL of
/// the title's after the executable) without counting; bit 1 says the
/// version arrives as major << 32 | minor; bit 2 checks options only.
const JOIN: u64 = 0x1;
const SPLIT_VERSION: u64 = 0x2;
const CHECK_ONLY: u64 = 0x4;

/// Initialisations not yet undone, and the version the first one gave.
static INITIALIZED: Mutex<(u64, (u64, u64))> = Mutex::new((0, (0, 0)));

pub extern "system" fn InitializeApiImpl(version: u64, build: u64) -> HRESULT {
    initialize(version, build, 0, std::ptr::null())
}

pub extern "system" fn InitializeApiImplEx(version: u64, build: u64, flags: u64) -> HRESULT {
    initialize(version, build, flags, std::ptr::null())
}

/// The stub prefers this one when present. It passes the same three values
/// and a pointer to the title's initialisation options, null unless the
/// title initialises with options (the title does not).
pub extern "system" fn InitializeApiImplEx2(
    version: u64,
    build: u64,
    flags: u64,
    options: *const c_void,
) -> HRESULT {
    initialize(version, build, flags, options)
}

/// As the console's xgameruntime.dll: unknown flags are E_INVALIDARG, a
/// title built for a later runtime is refused, a check-only call succeeds,
/// a join without an initialisation fails (reported as the console
/// reports it), and the rest count up for `UninitializeApiImpl`.
// shortcut: options (the fourth argument) are not compared with an earlier
// initialisation's; no title of ours passes any.
fn initialize(version: u64, build: u64, flags: u64, options: *const c_void) -> HRESULT {
    tracing::info!(version, build, flags, ?options, "InitializeApiImpl");
    if flags & !0xf != 0 {
        return xasync::E_INVALIDARG;
    }
    let wanted = if flags & SPLIT_VERSION != 0 {
        (version >> 32, version & 0xffff_ffff)
    } else {
        (version, build)
    };
    if wanted > RUNTIME_VERSION {
        tracing::warn!(?wanted, runtime = ?RUNTIME_VERSION, "the title wants a newer runtime");
        return VERSION_MISMATCH;
    }
    if flags & CHECK_ONLY != 0 {
        return S_OK;
    }
    let mut state = INITIALIZED.lock().unwrap_or_else(|e| e.into_inner());
    if state.0 == 0 {
        if flags & JOIN != 0 {
            XErrorReport(NOT_INITIALIZED, c"the runtime is not initialised".as_ptr());
            return NOT_INITIALIZED;
        }
        state.1 = wanted;
    }
    if flags & JOIN == 0 {
        state.0 += 1;
    }
    S_OK
}

/// Undoes one counted initialisation; nothing when there is none.
pub extern "system" fn UninitializeApiImpl() {
    let mut state = INITIALIZED.lock().unwrap_or_else(|e| e.into_inner());
    state.0 = state.0.saturating_sub(1);
    tracing::info!(left = state.0, "UninitializeApiImpl");
}

/// The object for an implemented (provider, interface id); others fail,
/// each pair logged once.
pub extern "system" fn QueryApiImpl(
    provider: *const Guid,
    iid: *const Guid,
    out: *mut *mut c_void,
) -> HRESULT {
    static QUERIED: Mutex<Vec<(Guid, Guid)>> = Mutex::new(Vec::new());
    // SAFETY: the title passes pointers to GUIDs (null is tolerated).
    let (Some(&provider), Some(&iid)) = (unsafe { provider.as_ref() }, unsafe { iid.as_ref() })
    else {
        return NOT_SUPPORTED;
    };
    #[cfg(feature = "diagnostics")]
    if tracing::enabled!(tracing::Level::DEBUG) {
        // Each runtime call site once: which title code uses which area, when.
        static CALLS: Mutex<Vec<String>> = Mutex::new(Vec::new());
        let from = super::kernel::callers();
        let mut calls = CALLS.lock().unwrap_or_else(|e| e.into_inner());
        if !calls.contains(&from) {
            tracing::debug!(%provider, %iid, %from, "runtime call");
            calls.push(from);
        }
    }
    let mut pairs = AREAS.iter().flat_map(|area| area.iter());
    let found = pairs
        .clone()
        .find(|(p, i, _)| (*p, *i) == (provider, iid))
        .or_else(|| {
            pairs
                .clone()
                .find(|(p, _, _)| *p == provider && iid == IUNKNOWN)
        });
    if let Some(&(_, _, object)) = found
        && !out.is_null()
    {
        // SAFETY: checked non-null; the title passes room for a pointer.
        unsafe { out.write((object as *const Object).cast_mut().cast()) };
        return S_OK;
    }
    let mut queried = QUERIED.lock().unwrap_or_else(|e| e.into_inner());
    if !queried.contains(&(provider, iid)) {
        queried.push((provider, iid));
        tracing::warn!(%provider, %iid, "runtime API area not implemented");
    }
    // As the console's runtime: a provider it has answers an interface id
    // it lacks with E_NOINTERFACE and a cleared pointer; an unknown
    // provider is NOT_SUPPORTED, the pointer untouched.
    // shortcut: providers the console has that we lack altogether still
    // answer NOT_SUPPORTED.
    if pairs.any(|(p, _, _)| *p == provider) {
        if !out.is_null() {
            // SAFETY: checked non-null.
            unsafe { out.write(std::ptr::null_mut()) };
        }
        return E_NOINTERFACE;
    }
    NOT_SUPPORTED
}

pub extern "system" fn XErrorReport(hr: HRESULT, message: *const c_char) {
    let message = if message.is_null() {
        "".into()
    } else {
        // SAFETY: a non-null message is a NUL-terminated string.
        unsafe { CStr::from_ptr(message) }.to_string_lossy()
    };
    tracing::warn!(hr = format_args!("{:#010x}", hr as u32), %message, "XErrorReport");
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "xgameruntime",
        dlls: &["xgameruntime.dll"],
        exports: vec![
            hle_export!(XGameRuntimeInitialize),
            hle_export!(XGameRuntimeUninitialize),
            hle_export!(InitializeApiImpl),
            hle_export!(InitializeApiImplEx),
            hle_export!(InitializeApiImplEx2),
            hle_export!(UninitializeApiImpl),
            hle_export!(QueryApiImpl),
            hle_export!(XErrorReport),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::kernel::{FreeLibrary, GetProcAddress, LoadLibraryExA};

    #[test]
    fn initialisation_checks_flags_and_version_as_the_console_does() {
        let ours = 0x6336 << 32 | 0x2712;
        assert_eq!(InitializeApiImplEx(ours, 0, 0x12), xasync::E_INVALIDARG);
        assert_eq!(
            InitializeApiImplEx(ours + 1, 0, SPLIT_VERSION),
            VERSION_MISMATCH
        );
        assert_eq!(InitializeApiImplEx(0x6337, 0, 0), VERSION_MISMATCH);
        assert_eq!(
            InitializeApiImplEx(ours, 0, SPLIT_VERSION | CHECK_ONLY),
            S_OK
        );

        // A known provider without the interface: E_NOINTERFACE, cleared.
        let mut out = std::ptr::dangling_mut();
        let other = Guid::new(1, 2, 3, [4; 8]);
        assert_eq!(
            QueryApiImpl(&xasync::PROVIDER, &other, &mut out),
            E_NOINTERFACE
        );
        assert!(out.is_null());
        assert_eq!(QueryApiImpl(&xasync::PROVIDER, &IUNKNOWN, &mut out), S_OK);
        assert!(!out.is_null());
    }

    #[test]
    fn title_stub_loads_the_runtime_dynamically() {
        let load = |name: &CStr| LoadLibraryExA(name.as_ptr().cast(), std::ptr::null_mut(), 0x800);
        let runtime = load(c"xgameruntime.dll");
        assert!(!runtime.is_null());
        let proc = |name: &CStr| GetProcAddress(runtime, name.as_ptr().cast());
        for name in [
            c"InitializeApiImpl",
            c"InitializeApiImplEx",
            c"InitializeApiImplEx2",
            c"QueryApiImpl",
            c"UninitializeApiImpl",
            c"XErrorReport",
        ] {
            assert!(!proc(name).is_null(), "{name:?}");
        }
        assert!(proc(c"NotAnExport").is_null());
        let ordinal = std::ptr::without_provenance::<u8>(1);
        assert!(GetProcAddress(runtime, ordinal).is_null());

        type Init = extern "system" fn(u64, u64, u64) -> HRESULT;
        type Query = extern "system" fn(*const Guid, *const Guid, *mut *mut c_void) -> HRESULT;
        // SAFETY: the addresses are our functions with these signatures.
        let (init, query) = unsafe {
            (
                std::mem::transmute::<*mut c_void, Init>(proc(c"InitializeApiImplEx")),
                std::mem::transmute::<*mut c_void, Query>(proc(c"QueryApiImpl")),
            )
        };
        assert_eq!(init(1, 2, 2), S_OK);
        let guid = Guid {
            data1: 1,
            data2: 2,
            data3: 3,
            data4: [4; 8],
        };
        let untouched = 0x1234 as *mut c_void;
        let mut out = untouched;
        assert!(query(&guid, &guid, &mut out) < 0);
        assert_eq!(out, untouched);
        assert_eq!(FreeLibrary(runtime), 1);

        #[cfg(windows)]
        {
            let kernel32 = load(c"kernel32.dll");
            assert!(!kernel32.is_null(), "host DLLs still load from the host");
            assert!(!GetProcAddress(kernel32, c"GetTickCount".as_ptr().cast()).is_null());
        }
    }
}
