//! XPackage: the title's own installation, complete and launchable, with
//! every chunk ready; no other package (content, add-ons) is installed.
//! The package is the folder of the title's executable (`G:` to the title).
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::{c_char, c_void};
use std::path::Path;
use std::sync::OnceLock;

use super::xasync::{self, AsyncBlock};
use super::xgameruntime::{
    Guid, HRESULT, Interface, Methods, Object, S_OK, add_ref, missing, put, put_string, text, token,
};
use crate::runtime;

pub const PROVIDER: Guid = Guid::new(
    0xaf40_6016,
    0xe850,
    0x4aa8,
    [0xa8, 0x8d, 0x2f, 0x3d, 0xcb, 0x9d, 0xac, 0x7e],
);

/// As the title (2026) asks for it. Its slots up to 0xd0 take the
/// arguments the public API lists in the same order; 0x110 uninstalls a
/// package, 0x128/0x130 mount one with UI, 0x138 enumerates packages.
pub const IID_2026: Guid = Guid::new(
    0x0b9d_556e,
    0xab70,
    0x43f4,
    [0xba, 0xa0, 0x99, 0x99, 0xac, 0x4c, 0xb0, 0x40],
);

pub static OBJECT: Object = Object::new("package", &VTABLE);
pub const INTERFACES: &[Interface] = &[(PROVIDER, IID_2026, &OBJECT)];

/// The documented "could not resolve an identifier for the package".
pub const NO_PACKAGE: HRESULT = 0x8924_5208_u32 as i32;

/// Our installation's identifier: the title id in hex (titles keep it in 33
/// bytes and hand it back to us).
pub fn identifier() -> String {
    format!("{:08X}", runtime::title().id)
}

fn is_ours(id: *const c_char) -> bool {
    text(id).is_some_and(|id| id == identifier())
}

/// Bytes in the package folder, counted once.
fn package_bytes() -> u64 {
    fn size(dir: &Path) -> u64 {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return 0;
        };
        entries
            .flatten()
            .map(|e| match e.metadata() {
                Ok(m) if m.is_dir() => size(&e.path()),
                Ok(m) => m.len(),
                Err(_) => 0,
            })
            .sum()
    }
    static BYTES: OnceLock<u64> = OnceLock::new();
    *BYTES.get_or_init(|| {
        let exe = runtime::main_image().and_then(runtime::image_path);
        exe.as_deref().and_then(Path::parent).map_or(0, size)
    })
}

/// The package folder (the title's executable's).
fn package_dir() -> Option<std::path::PathBuf> {
    let exe = runtime::main_image().and_then(runtime::image_path)?;
    Path::new(&exe).parent().map(Path::to_path_buf)
}

/// The package's chunk ids, from its layout (`layout_*.xml`, `<Chunk
/// Id="...">`), in layout order; none without a layout.
fn chunk_ids() -> &'static [u32] {
    static IDS: OnceLock<Vec<u32>> = OnceLock::new();
    IDS.get_or_init(|| {
        let Some(dir) = package_dir() else {
            return Vec::new();
        };
        let layout = std::fs::read_dir(&dir).into_iter().flatten().flatten().find(|e| {
            let name = e.file_name().to_string_lossy().to_ascii_lowercase();
            name.starts_with("layout") && name.ends_with(".xml")
        });
        let Some(text) = layout.and_then(|e| std::fs::read_to_string(e.path()).ok()) else {
            return Vec::new();
        };
        let ids = layout_chunks(&text);
        tracing::info!(chunks = ids.len(), "package layout read");
        ids
    })
}

fn layout_chunks(text: &str) -> Vec<u32> {
    text.split("<Chunk")
        .skip(1)
        .filter_map(|tag| {
            let after = tag.split_once("Id=\"")?.1;
            after.split('"').next()?.trim().parse().ok()
        })
        .collect()
}

/// The package's languages (`MicrosoftGame.config`'s `<Resource
/// Language="...">`), in its order.
fn languages() -> &'static [String] {
    static LANGUAGES: OnceLock<Vec<String>> = OnceLock::new();
    LANGUAGES.get_or_init(|| {
        let config = package_dir().and_then(|dir| {
            std::fs::read_dir(&dir).into_iter().flatten().flatten().find(|e| {
                e.file_name().to_string_lossy().eq_ignore_ascii_case("MicrosoftGame.config")
            })
        });
        config
            .and_then(|e| std::fs::read_to_string(e.path()).ok())
            .map(|text| config_languages(&text))
            .unwrap_or_default()
    })
}

fn config_languages(text: &str) -> Vec<String> {
    text.split("<Resource ")
        .skip(1)
        .filter_map(|tag| Some(tag.split_once("Language=\"")?.1.split('"').next()?.to_string()))
        .collect()
}

/// The package language that best fits `user` (the user's locale): the
/// same, else one of the same language, else the package's first.
fn best_locale<'a>(user: &str, languages: &'a [String]) -> Option<&'a str> {
    let same = |l: &&String| l.eq_ignore_ascii_case(user);
    let base = |l: &String| l.split('-').next().unwrap_or(l).to_ascii_lowercase();
    let user_base = user.split('-').next().unwrap_or(user).to_ascii_lowercase();
    languages
        .iter()
        .find(same)
        .or_else(|| languages.iter().find(|l| base(l) == user_base))
        .or(languages.first())
        .map(String::as_str)
}

fn user_locale() -> String {
    #[cfg(windows)]
    {
        let mut name = [0u16; 85];
        // SAFETY: a buffer of LOCALE_NAME_MAX_LENGTH characters.
        let n = unsafe {
            windows_sys::Win32::Globalization::GetUserDefaultLocaleName(name.as_mut_ptr(), name.len() as i32)
        };
        if n > 1 {
            return String::from_utf16_lossy(&name[..n as usize - 1]);
        }
    }
    "en-US".into()
}

type This = *const Object;

extern "system" fn current_identifier(_: This, size: usize, buffer: *mut u8) -> HRESULT {
    put_string(&identifier(), size, buffer, std::ptr::null_mut())
}

extern "system" fn is_packaged(_: This) -> bool {
    true
}

/// Installation monitors all watch the same finished installation, so the
/// handle is a fixed address.
static MONITOR: u8 = 0;
fn monitor() -> *const c_void {
    (&raw const MONITOR).cast()
}

extern "system" fn create_monitor(
    _: This,
    id: *const c_char,
    _selector_count: u32,
    _selectors: *const c_void,
    _interval_ms: u32,
    _queue: *const c_void,
    out: *mut *const c_void,
) -> HRESULT {
    if !is_ours(id) {
        return NO_PACKAGE;
    }
    put(out, monitor())
}
extern "system" fn close_monitor(_: This, _: *const c_void) {}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    pub total_bytes: u64,
    pub installed_bytes: u64,
    pub launch_bytes: u64,
    pub launchable: bool,
    pub completed: bool,
}

extern "system" fn progress(_: This, _: *const c_void, out: *mut Progress) {
    let bytes = package_bytes();
    put(
        out,
        Progress {
            total_bytes: bytes,
            installed_bytes: bytes,
            launch_bytes: bytes,
            launchable: true,
            completed: true,
        },
    );
}

/// The installation is complete, so progress never changes.
extern "system" fn register_progress(
    _: This,
    _monitor: *const c_void,
    _context: *mut c_void,
    _callback: *const c_void,
    out: *mut u64,
) -> HRESULT {
    put(out, token())
}

/// The completed installation has no pending progress callbacks to wait for.
extern "system" fn unregister_progress(
    _: This,
    _monitor: *const c_void,
    _token: u64,
    _wait: bool,
) -> bool {
    true
}

/// 0: ready.
// shortcut: every selector counts as installed, including ones the
// package's layout does not have (the console answers those with an
// invalid-selector error); the title asks only about its own
// chunks, which a full install has.
extern "system" fn find_availability(
    _: This,
    id: *const c_char,
    _selector_count: u32,
    _selectors: *const c_void,
    out: *mut u32,
) -> HRESULT {
    if !is_ours(id) {
        return NO_PACKAGE;
    }
    put(out, 0)
}

extern "system" fn change_order(
    _: This,
    id: *const c_char,
    _count: u32,
    _selectors: *const c_void,
) -> HRESULT {
    if is_ours(id) { S_OK } else { NO_PACKAGE }
}

static INSTALL: u8 = 0;

/// Everything is installed: done at once, the result is a monitor.
extern "system" fn install_chunks_async(
    _: This,
    id: *const c_char,
    _count: u32,
    _selectors: *const c_void,
    _interval_ms: u32,
    _suppress_confirmation: bool,
    block: *mut AsyncBlock,
) -> HRESULT {
    let result = if is_ours(id) {
        Ok(xasync::bytes(monitor()))
    } else {
        Err(NO_PACKAGE)
    };
    xasync::finished_with(block, (&raw const INSTALL).cast(), result)
}
extern "system" fn install_chunks_result(
    _: This,
    block: *mut AsyncBlock,
    out: *mut *const c_void,
) -> HRESULT {
    xasync::result(block, (&raw const INSTALL).cast(), out)
}

/// Nothing to download.
extern "system" fn estimate_download(
    _: This,
    id: *const c_char,
    _count: u32,
    _selectors: *const c_void,
    bytes: *mut u64,
    confirm: *mut bool,
) -> HRESULT {
    if !is_ours(id) {
        return NO_PACKAGE;
    }
    if !confirm.is_null() {
        // SAFETY: checked non-null.
        unsafe { confirm.write(false) };
    }
    put(bytes, 0)
}

// shortcut: accepted but nothing is removed (the package folder is the
// user's), so the chunks stay ready; the title's one call site
// (title+0x293b4b0, its chunk manager) would then see them still installed.
extern "system" fn uninstall_chunks(
    _: This,
    id: *const c_char,
    _count: u32,
    _selectors: *const c_void,
) -> HRESULT {
    if is_ours(id) { S_OK } else { NO_PACKAGE }
}

/// Mount handles only come from mounting another package, and there is
/// none.
extern "system" fn mount_path(_: This, _: *const c_void, _: usize, _: *mut u8) -> HRESULT {
    xasync::E_INVALIDARG
}
extern "system" fn close_mount(_: This, _: *const c_void) {}

/// Only other packages are uninstalled this way, and none is installed.
extern "system" fn uninstall_package(_: This, id: *const c_char) -> bool {
    tracing::info!(
        id = text(id).unwrap_or_default(),
        "package uninstall refused"
    );
    false
}

/// 0x40: progress is final, so there is nothing to bring up to date.
extern "system" fn update_monitor(_: This, _: *const c_void) {}

/// 0x58 (`XPackageGetUserLocale`): the package language that best fits the
/// user's locale (en-US when the package lists none).
extern "system" fn get_user_locale(_: This, size: usize, out: *mut u8) -> HRESULT {
    let user = user_locale();
    let locale = best_locale(&user, languages()).unwrap_or("en-US").to_string();
    tracing::debug!(user, locale, "package user locale");
    put_string(&locale, size, out, std::ptr::null_mut())
}

/// `XPackageChunkSelector`: a type (0 language, 1 tag, 2 chunk, 3 feature)
/// and a chunk id or a string.
#[repr(C)]
struct Selector {
    kind: u32,
    value: usize,
}

const CHUNK_SELECTOR: u32 = 2;

type AvailabilityCallback = extern "system" fn(*mut c_void, *const Selector, u32) -> bool;

/// 0x68 (`XPackageEnumerateChunkAvailability`): every chunk of the layout,
/// ready, for the chunk type; the layout's chunks name no languages, tags
/// or features, so those types list nothing.
extern "system" fn enumerate_availability(
    _: This,
    id: *const c_char,
    kind: u32,
    context: *mut c_void,
    callback: Option<AvailabilityCallback>,
) -> HRESULT {
    if !is_ours(id) {
        return NO_PACKAGE;
    }
    let Some(callback) = callback else {
        return xasync::E_INVALIDARG;
    };
    if kind == CHUNK_SELECTOR {
        for &chunk in chunk_ids() {
            let selector = Selector { kind, value: chunk as usize };
            if !callback(context, &selector, 0) {
                break;
            }
        }
    }
    S_OK
}

/// 0x78 (`XPackageInstallChunks`): everything is installed; the monitor
/// watches the finished installation.
extern "system" fn install_chunks(
    _: This,
    id: *const c_char,
    _count: u32,
    _selectors: *const c_void,
    _interval_ms: u32,
    _suppress_confirmation: bool,
    out: *mut *const c_void,
) -> HRESULT {
    if !is_ours(id) {
        return NO_PACKAGE;
    }
    put(out, monitor())
}

/// 0xb8 (`XPackageMount`, the older synchronous mount): no other package
/// is installed.
extern "system" fn mount(_: This, id: *const c_char, _: *mut *const c_void) -> HRESULT {
    tracing::info!(id = text(id).unwrap_or_default(), "package mount: not installed");
    NO_PACKAGE
}

/// 0xc0 (`XPackageGetMountPathSize`): as `mount_path`, there are no mounts.
extern "system" fn mount_path_size(_: This, _: *const c_void, _: *mut usize) -> HRESULT {
    xasync::E_INVALIDARG
}

static MOUNT: u8 = 0;

extern "system" fn mount_async(_: This, id: *const c_char, block: *mut AsyncBlock) -> HRESULT {
    tracing::info!(
        id = text(id).unwrap_or_default(),
        "package mount: not installed"
    );
    xasync::finished(block, (&raw const MOUNT).cast(), NO_PACKAGE)
}
extern "system" fn mount_result(
    _: This,
    block: *mut AsyncBlock,
    out: *mut *const c_void,
) -> HRESULT {
    xasync::result(block, (&raw const MOUNT).cast(), out)
}

/// Titles enumerate content packages (kind 1); there are none.
// shortcut: the game kind (0) also lists nothing, not this installation;
// The title enumerates only content (kind 1, title+0x2937bf0).
extern "system" fn enumerate(
    _: This,
    kind: u32,
    _scope: u32,
    _context: *mut c_void,
    _callback: *const c_void,
) -> HRESULT {
    tracing::debug!(kind, "packages enumerated: none");
    S_OK
}

static VTABLE: Methods<40> = Methods([
    missing::<0> as *const (),
    add_ref as *const (),
    add_ref as *const (),
    current_identifier as *const (),    // 0x18
    is_packaged as *const (),           // 0x20
    create_monitor as *const (),        // 0x28
    close_monitor as *const (),         // 0x30
    progress as *const (),              // 0x38
    update_monitor as *const (),        // 0x40
    register_progress as *const (),     // 0x48
    unregister_progress as *const (),   // 0x50
    get_user_locale as *const (),       // 0x58
    find_availability as *const (),     // 0x60
    enumerate_availability as *const (), // 0x68
    change_order as *const (),          // 0x70
    install_chunks as *const (),        // 0x78
    install_chunks_async as *const (),  // 0x80
    install_chunks_result as *const (), // 0x88
    estimate_download as *const (),     // 0x90
    uninstall_chunks as *const (),      // 0x98
    missing::<20> as *const (),         // 0xa0
    missing::<21> as *const (),         // 0xa8
    missing::<22> as *const (),         // 0xb0
    mount as *const (),                 // 0xb8 (deprecated)
    mount_path_size as *const (),       // 0xc0
    mount_path as *const (),            // 0xc8
    close_mount as *const (),           // 0xd0
    missing::<27> as *const (),         // 0xd8
    missing::<28> as *const (),         // 0xe0 enumerate packages (older)
    missing::<29> as *const (),         // 0xe8
    missing::<30> as *const (),         // 0xf0
    missing::<31> as *const (),         // 0xf8
    missing::<32> as *const (),         // 0x100
    missing::<33> as *const (),         // 0x108
    uninstall_package as *const (),     // 0x110
    missing::<35> as *const (),         // 0x118
    missing::<36> as *const (),         // 0x120
    mount_async as *const (),           // 0x128
    mount_result as *const (),          // 0x130
    enumerate as *const (),             // 0x138
]);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::xgameruntime::method;

    #[test]
    fn completed_installation_unregisters_progress_immediately() {
        type Register =
            extern "system" fn(This, *const c_void, *mut c_void, *const c_void, *mut u64) -> HRESULT;
        type Unregister = extern "system" fn(This, *const c_void, u64, bool) -> bool;
        extern "system" fn callback(_: *mut c_void, _: *const c_void) {}

        let this: This = &OBJECT;
        let register = method::<Register>(&OBJECT, 0x48);
        assert_eq!(VTABLE.0[0x50 / 8], unregister_progress as *const ());
        let unregister = method::<Unregister>(&OBJECT, 0x50);
        for wait in [false, true] {
            let mut token = 0;
            assert_eq!(
                register(
                    this,
                    monitor(),
                    std::ptr::null_mut(),
                    callback as *const c_void,
                    &mut token
                ),
                S_OK
            );
            assert_ne!(token, 0);
            assert!(unregister(this, monitor(), token, wait));
        }
    }

    #[test]
    fn the_installation_is_complete_and_alone() {
        let this: This = &OBJECT;
        type Id = extern "system" fn(This, usize, *mut u8) -> HRESULT;
        let mut id = [0u8; 33];
        assert_eq!(method::<Id>(&OBJECT, 0x18)(this, 33, id.as_mut_ptr()), S_OK);
        let len = id.iter().position(|&b| b == 0).unwrap();
        assert_eq!(std::str::from_utf8(&id[..len]).unwrap(), identifier());
        assert!(method::<extern "system" fn(This) -> bool>(&OBJECT, 0x20)(
            this
        ));

        type Find =
            extern "system" fn(This, *const c_char, u32, *const c_void, *mut u32) -> HRESULT;
        let mut availability = 9;
        let ours = format!("{}\0", identifier());
        let find = method::<Find>(&OBJECT, 0x60);
        let none = std::ptr::null();
        assert_eq!(
            find(this, ours.as_ptr().cast(), 0, none, &mut availability),
            S_OK
        );
        assert_eq!(availability, 0, "ready");
        assert_eq!(
            find(this, c"dlc".as_ptr(), 0, none, &mut availability),
            NO_PACKAGE
        );

        type Progress_ = extern "system" fn(This, *const c_void, *mut Progress);
        let mut p = Progress {
            total_bytes: 1,
            installed_bytes: 0,
            launch_bytes: 0,
            launchable: false,
            completed: false,
        };
        method::<Progress_>(&OBJECT, 0x38)(this, monitor(), &mut p);
        assert!(p.completed && p.launchable && p.installed_bytes == p.total_bytes);

        type Mount = extern "system" fn(This, *const c_char, *mut AsyncBlock) -> HRESULT;
        let mut block = AsyncBlock {
            queue: std::ptr::null(),
            context: std::ptr::null_mut(),
            callback: None,
            internal: [0; 4],
        };
        assert_eq!(
            method::<Mount>(&OBJECT, 0x128)(this, c"dlc".as_ptr(), &mut block),
            S_OK
        );
        assert_eq!(xasync::XAsyncGetStatus(&mut block, true), NO_PACKAGE);
    }

    #[test]
    fn layout_chunks_and_config_languages() {
        let layout = r#"<Package>
  <Chunk Id="1000">
    <FileGroup SourcePath=".\" DestinationPath="\" Include="a.rpf" />
  </Chunk>
  <Chunk Id="3004" Marker="Launch">
  </Chunk>
</Package>"#;
        assert_eq!(layout_chunks(layout), [1000, 3004]);
        let config = r#"<Resources><Resource Language="en-US" /><Resource Language="fr-FR" /></Resources>"#;
        let languages = config_languages(config);
        assert_eq!(languages, ["en-US", "fr-FR"]);
        assert_eq!(best_locale("fr-FR", &languages), Some("fr-FR"));
        assert_eq!(best_locale("fr-CA", &languages), Some("fr-FR"));
        assert_eq!(best_locale("de-DE", &languages), Some("en-US"));
        assert_eq!(best_locale("de-DE", &[]), None);
    }
}
