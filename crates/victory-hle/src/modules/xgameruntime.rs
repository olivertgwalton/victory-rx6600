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

impl Guid {
    /// The id as one number, for ordering and quick comparison.
    pub const fn key(&self) -> u128 {
        let d = self.data4;
        (self.data1 as u128) << 96
            | (self.data2 as u128) << 80
            | (self.data3 as u128) << 64
            | u64::from_be_bytes(d) as u128
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

/// `AREAS` flattened at compile time, ids as numbers, in `AREAS` order (the
/// title's most frequent areas first): one 32-byte compare per entry.
const COUNT: usize = {
    let (mut n, mut i) = (0, 0);
    while i < AREAS.len() {
        n += AREAS[i].len();
        i += 1;
    }
    n
};
static KEYS: [(u128, u128, &Object); COUNT] = {
    let mut keys = [(0, 0, AREAS[0][0].2); COUNT];
    let (mut n, mut i) = (0, 0);
    while i < AREAS.len() {
        let mut j = 0;
        while j < AREAS[i].len() {
            let (p, iid, object) = AREAS[i][j];
            keys[n] = (p.key(), iid.key(), object);
            n += 1;
            j += 1;
        }
        i += 1;
    }
    keys
};

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
    let (p, i) = (provider.key(), iid.key());
    let found = KEYS
        .iter()
        .find(|e| e.0 == p && e.1 == i)
        .or_else(|| KEYS.iter().find(|e| e.0 == p && iid == IUNKNOWN))
        .map(|e| e.2);
    if let Some(object) = found
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
    if KEYS.iter().any(|e| e.0 == p) {
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

    fn guid(text: &str) -> Guid {
        let hex: String = text.chars().filter(|c| *c != '-').collect();
        let b = |i: usize| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap();
        Guid::new(
            u32::from_str_radix(&hex[0..8], 16).unwrap(),
            u16::from_str_radix(&hex[8..12], 16).unwrap(),
            u16::from_str_radix(&hex[12..16], 16).unwrap(),
            std::array::from_fn(|i| b(8 + i)),
        )
    }

    /// The console xgameruntime.dll's providers, in its table's order
    /// (0x1801a2450, walked by `QueryApiImpl` at 0x180017a00), each with
    /// the RVA of its object's QueryInterface and the interface ids that
    /// function accepts besides IUnknown ("self" is the provider's own id).
    /// Providers 22 and 24 answer through a QueryInterface not found yet.
    const CONSOLE: [(&str, u32, &[&str]); 25] = [
        ("8ca467f7-22e8-4096-8456-bb8aa13f79d8", 0xa9c70, &["self"]),
        (
            "01acd177-91f9-4763-a38e-ccbb55ce32e0",
            0x97040,
            &[
                "self",
                "5131d685-4394-4ee6-8c18-bfb5d4aef1ff",
                "26f3c674-a2fe-44fa-b6c4-a323bc94ff53",
                "079415e3-6727-437f-8e9d-8f8f9b2439f7",
                "1bf2f8c5-d507-4e52-bb05-f726d0e71161",
                "eb9bf948-18dc-4d82-bbcc-40e0a809c4c0",
                "3d5d819d-3fce-4ca2-b167-e4a7f7e96630",
                "cef4fac0-7676-4a94-a119-4c43f9eb5b74",
            ],
        ),
        ("073b7dcb-1fcf-4030-94be-e3c9eb623428", 0xbe730, &["self"]),
        (
            "973a344e-24bf-4d0f-8457-56c534892b29",
            0xf7030,
            &[
                "self",
                "2549f142-6419-4a06-97b5-931aab7c2f34",
                "50849859-0ad8-4f81-80e4-5bc78626f852",
            ],
        ),
        (
            "704c3f58-e629-4cc2-b197-30511b996fe2",
            0x265ec,
            &[
                "self",
                "1bfff3af-f14a-40a3-8e35-9ada906593f9",
                "ab4ae4fb-6508-4950-a032-45fd4bf8c43b",
                "cd7bb1b6-64ab-42e2-b11b-2e5e45fb9a56",
            ],
        ),
        (
            "3e241977-6237-41e9-8559-102c2d9983f1",
            0x1a1fc,
            &["self", "d722b373-8c4d-4692-9e51-d6ad9b37aa7d"],
        ),
        ("1b339674-328d-4283-a200-3171f18d3639", 0x1fa10, &["self"]),
        (
            "dfcd4649-4ff8-4043-ba07-35d607df98b0",
            0x57740,
            &[
                "self",
                "eaf669df-5542-4590-99a3-8dc061f837cc",
                "ade7eba1-2093-42ce-a544-a523a66790e0",
                "36a03122-9ea3-4a3a-a8a4-899cfd85d7db",
                "72c4e70f-4a67-481b-bb84-aa72eda14fe5",
                "fc0b9bd0-8898-4643-bee0-4ee6ee034cb6",
                "6eeaa73e-9669-43ad-a2c7-d0da4e1f50a1",
                "121248e5-0f5e-48d7-854a-7c926ae89162",
                "a26365fb-fb36-415c-89c7-ec763880c1f5",
                "22e851e4-d503-4d4f-a4c8-0508e3470d45",
            ],
        ),
        (
            "7d824997-10dc-45ab-86b7-2737767c0bf1",
            0xd76d0,
            &["self", "0cc6a956-e7e1-4fdf-9341-9d5da649ebc8"],
        ),
        (
            "af406016-e850-4aa8-a88d-2f3dcb9dac7e",
            0x61080,
            &[
                "self",
                "0b9d556e-ab70-43f4-baa0-9999ac4cb040",
                "e2a4734b-2f4a-456d-aa8f-d065e04fb209",
                "f92d8712-2b27-4d8a-bf01-11a6f8e3eb42",
                "3720de07-e8e4-44a3-ad32-b359e8adbe55",
                "be3f3e9f-c4b3-424d-9a95-d1af060559a5",
                "9899272d-a086-4c47-b136-6ebe37d89a94",
                "215905e2-dda9-4c5a-9ad2-9839e0b06679",
                "33515fb4-e396-4821-bb6d-bf5a445c320c",
            ],
        ),
        (
            "f4faf4d4-2d04-4fce-b3e0-474a713a3e84",
            0x6c380,
            &[
                "self",
                "41a4e10c-5a7e-41d9-8c37-37bde62a07d6",
                "d29411df-0794-4553-8b27-95fc02d0f75d",
            ],
        ),
        (
            "37e56907-2f10-41e8-b72f-36edb185331a",
            0xe57d0,
            &["self", "bf2346b2-39af-4658-b5ea-44713c7e83b3"],
        ),
        ("186d5592-a72d-45fb-9560-11aed0e6647a", 0xeb640, &["self"]),
        (
            "a4f1aee2-4bf1-4485-b008-a7c26d52ac27",
            0xeb540,
            &[
                "self",
                "22e672d7-b4e3-406c-bd50-8f0d25236f9e",
                "2bbca60a-619c-4fe1-812e-fb5c1dbdcf51",
                "3a949778-772e-4799-bdea-0a6639e96baa",
            ],
        ),
        (
            "03f0fe74-fdd9-4e5c-b630-f9339c47acc5",
            0xc24a0,
            &["self", "35f07670-706e-4bfb-9476-090798c5ebf3"],
        ),
        ("b884675d-b738-4a9c-815d-9a9a1e0c6c9b", 0xc1c00, &["self"]),
        (
            "0dd112ac-7c24-448c-b92b-3960fb5bd30c",
            0x715fc,
            &[
                "self",
                "b09d803c-2414-4a05-82c6-66dfdc9e9a44",
                "5c48dedf-0b67-4492-a4b5-6829b8e796e1",
                "de3dbdd4-0b37-4bdb-a10e-acf3a354d06a",
                "2d42fea5-e71d-4b76-97cd-c50afbb3ae5d",
                "60b09f4e-1b85-45b1-826c-169118e230e1",
            ],
        ),
        (
            "e349bd1a-fc20-4e40-b99c-4178cc6b409f",
            0xf6320,
            &[
                "self",
                "1861cf2e-e18b-4834-a9f5-b4a4e6efb4cf",
                "dadc2895-34b0-4ef5-a83e-45114d629b80",
                "67ce4bfc-b1d1-4ac7-bc3a-cb9219a97a85",
                "6fd71f09-7513-49f0-89bc-bfaf5df6f852",
            ],
        ),
        (
            "0651aae2-4012-4077-bf84-8b9097090e2c",
            0x23238,
            &["self", "014d1cc3-bcfe-41ff-b2f0-e1ef07155828"],
        ),
        ("bbfbdcc7-bfe7-409b-a5ca-edf054960b4d", 0xc4fe0, &["self"]),
        (
            "0a2192aa-b2d5-4d58-83be-383b6d80799e",
            0x10ddf0,
            &[
                "self",
                "57786622-6605-46d0-b917-0f22bbcd9c52",
                "5f5e5169-746c-4001-ad1c-da728d01c9eb",
                "8aff07f5-a1bf-4db8-80a5-31cca0de51b7",
                "261ed4f9-10e9-4f2c-ae05-35821abcd9d0",
                "7ac0b612-6354-49e8-8e16-fcfcaf681895",
                "7d3371ca-00d6-4a1f-a2e1-d330cd5b36e8",
                "aebdfe78-4394-49ff-ba3a-001956457044",
            ],
        ),
        ("8836fe87-edb9-4fe3-8dad-05f0d2cd5b40", 0x13530, &["self"]),
        ("95fd18d2-74dd-4d7c-aa1b-0b51827665d6", 0, &[]),
        ("0eae2136-8aa2-4102-9da5-62ca56115aac", 0xf04a0, &["self"]),
        ("7f0fe8b8-e075-49ab-9aa7-a1e065489a9e", 0, &[]),
    ];

    /// Console providers the layer has no area for: `QueryApiImpl` answers
    /// them NOT_SUPPORTED where the console hands an object.
    const MISSING: [usize; 6] = [0, 12, 20, 22, 23, 24];

    /// Every API family the console runtime dispatches, through the
    /// layer's `QueryApiImpl`, against what the console's would answer.
    #[test]
    fn dispatch_covers_every_console_provider() {
        let mut served = 0;
        let mut unserved = Vec::new();
        for (index, (provider, _qi, iids)) in CONSOLE.iter().enumerate() {
            let provider = guid(provider);
            let ids: Vec<Guid> = iids
                .iter()
                .map(|i| if *i == "self" { provider } else { guid(i) })
                .chain([IUNKNOWN])
                .collect();
            let known = AREAS
                .iter()
                .flat_map(|a| a.iter())
                .any(|(p, _, _)| *p == provider);
            assert_eq!(
                !known,
                MISSING.contains(&index),
                "provider {index} {provider}"
            );
            for iid in &ids {
                let untouched = std::ptr::dangling_mut();
                let mut out = untouched;
                let hr = QueryApiImpl(&provider, iid, &mut out);
                match hr {
                    S_OK => {
                        assert!(!out.is_null());
                        served += 1;
                    }
                    E_NOINTERFACE => {
                        assert!(known && out.is_null());
                        unserved.push((index, *iid));
                    }
                    _ => {
                        assert!(
                            !known && hr == NOT_SUPPORTED && out == untouched,
                            "{provider} {iid}"
                        );
                        unserved.push((index, *iid));
                    }
                }
            }
            // Any other interface id of a provider: E_NOINTERFACE, cleared.
            let mut out = std::ptr::dangling_mut();
            let other = Guid::new(0x1234, 5, 6, [7; 8]);
            let hr = QueryApiImpl(&provider, &other, &mut out);
            assert_eq!(hr, if known { E_NOINTERFACE } else { NOT_SUPPORTED });
        }
        // Everything the layer serves the console serves too.
        for (provider, iid, _) in AREAS.iter().flat_map(|a| a.iter()) {
            let console = CONSOLE.iter().find(|(p, _, _)| guid(p) == *provider);
            let (p, _, iids) =
                console.unwrap_or_else(|| panic!("{provider} is no console provider"));
            assert!(
                iids.iter().any(|i| (*i == "self" && guid(p) == *iid) || (*i != "self" && guid(i) == *iid)),
                "{provider} {iid} is not answered by the console"
            );
        }
        assert_eq!(served, 45, "pairs answered with an object");
        assert_eq!(
            unserved.len(),
            58,
            "pairs the console answers that we do not: {unserved:?}"
        );
    }

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

    /// Every runtime call of a title queries its area first. Run with
    /// `--ignored --nocapture bench_` on a release test build.
    #[test]
    #[ignore]
    fn bench_query_api_impl() {
        use crate::modules::gameinput::tests::time;
        let mut out = std::ptr::null_mut();
        let first = AREAS[0][0];
        let last = *AREAS.last().unwrap().last().unwrap();
        time("QueryApiImpl, first area", 1_000_000, || {
            std::hint::black_box(QueryApiImpl(&first.0, &first.1, &mut out));
        });
        time("QueryApiImpl, last area", 1_000_000, || {
            std::hint::black_box(QueryApiImpl(&last.0, &last.1, &mut out));
        });
        let other = Guid::new(9, 9, 9, [9; 8]);
        time("QueryApiImpl, unknown interface", 1_000_000, || {
            std::hint::black_box(QueryApiImpl(&first.0, &other, &mut out));
        });
        time("QueryApiImpl, unknown provider", 1_000_000, || {
            std::hint::black_box(QueryApiImpl(&other, &other, &mut out));
        });
    }
}
