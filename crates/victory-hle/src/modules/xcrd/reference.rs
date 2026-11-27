//! The console's `XCRDAPI.dll` as a test oracle. It is loaded from
//! `VICTORY_REFERENCE_DIR` (never from the repository) and its imports of
//! the driver and the system are pointed at a scripted fake driver; the
//! port's [`super::sys`] table is pointed at the same fake. A test then calls
//! an export of each with the same arguments and compares what comes back,
//! what lands in the out-parameters, and every call the fake saw (the
//! `SRB_IO_CONTROL` bytes sent, files opened, messages, sleeps).

use std::ffi::c_void;
use std::sync::{Mutex, MutexGuard, OnceLock};

use super::sys::{self, Handle, IoStatus, Sys};

/// One call to the driver.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Call {
    pub handle: usize,
    pub code: u32,
    pub input: Vec<u8>,
}

/// What a side did, for comparison.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Record {
    pub calls: Vec<Call>,
    pub opened: Vec<String>,
    pub messages: Vec<String>,
    pub sleeps: Vec<u32>,
    pub closed: Vec<usize>,
    /// File-system calls, described.
    pub fs: Vec<String>,
}

type Open = Box<dyn FnMut(&str) -> Result<usize, u32> + Send>;
/// Answers a driver call: may rewrite the buffer (in and out are one), and
/// returns the NTSTATUS.
type Reply = Box<dyn FnMut(&Call, &mut [u8]) -> i32 + Send>;

struct Driver {
    record: Record,
    open: Open,
    reply: Reply,
    fs: Fs,
}

/// A directory entry the fake file system lists.
#[derive(Clone, Debug)]
pub struct Entry {
    pub name: String,
    pub attributes: u32,
    pub size: u64,
    pub time: u64,
}

/// The fake file system: a drive type, one listing (or the error listing
/// gives), one file's bytes (every handle reads the same), and errors to
/// give for seeks and reads.
#[derive(Clone, Debug)]
pub struct Fs {
    pub drive_type: u32,
    pub entries: Result<Vec<Entry>, u32>,
    pub content: Vec<u8>,
    pub seek_error: Option<u32>,
    pub read_error: Option<u32>,
    next: usize,
    position: u64,
}

impl Default for Fs {
    fn default() -> Fs {
        Fs { drive_type: 1, entries: Err(3), content: Vec::new(), seek_error: None, read_error: None, next: 0, position: 0 }
    }
}

/// `WIN32_FIND_DATAW` for an entry; the times step by one.
fn find_data(e: &Entry, data: *mut c_void) {
    let mut b = [0u8; 0x250];
    b[0..4].copy_from_slice(&e.attributes.to_le_bytes());
    for (i, at) in [4usize, 0xc, 0x14].into_iter().enumerate() {
        b[at..at + 8].copy_from_slice(&(e.time + i as u64).to_le_bytes());
    }
    b[0x1c..0x20].copy_from_slice(&((e.size >> 32) as u32).to_le_bytes());
    b[0x20..0x24].copy_from_slice(&(e.size as u32).to_le_bytes());
    b[0x24..0x28].copy_from_slice(&0x1234u32.to_le_bytes());
    b[0x28..0x2c].copy_from_slice(&0x5678u32.to_le_bytes());
    for (i, c) in e.name.encode_utf16().take(259).enumerate() {
        b[0x2c + 2 * i..0x2e + 2 * i].copy_from_slice(&c.to_le_bytes());
    }
    for (i, c) in "ALT~1".encode_utf16().enumerate() {
        b[0x234 + 2 * i..0x236 + 2 * i].copy_from_slice(&c.to_le_bytes());
    }
    // SAFETY: the caller's WIN32_FIND_DATAW.
    unsafe { std::ptr::copy_nonoverlapping(b.as_ptr(), data.cast(), b.len()) };
}

fn set_last_error(e: u32) {
    // SAFETY: plain.
    unsafe { windows_sys::Win32::Foundation::SetLastError(e) };
}

/// The handle FindFirstFileW gives.
pub const FIND: usize = FAKE + 0xf00;

unsafe extern "system" fn fake_drive_type(root: *const u16) -> u32 {
    // SAFETY: a NUL-terminated root.
    let root = unsafe { super::wide_str(root) };
    let mut d = driver();
    d.record.fs.push(format!("drive type {root}"));
    d.fs.drive_type
}

unsafe extern "system" fn fake_find_first(pattern: *const u16, data: *mut c_void) -> Handle {
    // SAFETY: a NUL-terminated pattern.
    let pattern = unsafe { super::wide_str(pattern) };
    let mut d = driver();
    d.record.fs.push(format!("find first {pattern}"));
    match d.fs.entries.clone() {
        Ok(list) if !list.is_empty() => {
            find_data(&list[0], data);
            d.fs.next = 1;
            FIND as Handle
        }
        Ok(_) => {
            set_last_error(2);
            -1isize as Handle
        }
        Err(e) => {
            set_last_error(e);
            -1isize as Handle
        }
    }
}

unsafe extern "system" fn fake_find_next(find: Handle, data: *mut c_void) -> i32 {
    let mut d = driver();
    d.record.fs.push(format!("find next {:#x}", find as usize));
    let list = d.fs.entries.clone().unwrap_or_default();
    if d.fs.next < list.len() {
        find_data(&list[d.fs.next], data);
        d.fs.next += 1;
        1
    } else {
        set_last_error(0x12);
        0
    }
}

unsafe extern "system" fn fake_find_close(find: Handle) -> i32 {
    driver().record.fs.push(format!("find close {:#x}", find as usize));
    1
}

unsafe extern "system" fn fake_seek(file: Handle, distance: i64, position: *mut i64, method: u32) -> i32 {
    let mut d = driver();
    d.record.fs.push(format!("seek {:#x} {distance:#x} {} {method}", file as usize, position.is_null()));
    if let Some(e) = d.fs.seek_error {
        set_last_error(e);
        return 0;
    }
    d.fs.position = distance as u64;
    1
}

unsafe extern "system" fn fake_read(file: Handle, buffer: *mut c_void, length: u32, read: *mut u32, overlapped: *mut c_void) -> i32 {
    let mut d = driver();
    let line = format!("read {:#x} {length:#x} at {:#x} {}", file as usize, d.fs.position, overlapped.is_null());
    d.record.fs.push(line);
    if let Some(e) = d.fs.read_error {
        set_last_error(e);
        return 0;
    }
    let from = (d.fs.position as usize).min(d.fs.content.len());
    let n = (d.fs.content.len() - from).min(length as usize);
    // SAFETY: the caller's buffer of `length` bytes.
    unsafe { std::ptr::copy_nonoverlapping(d.fs.content[from..].as_ptr(), buffer.cast(), n) };
    d.fs.position += n as u64;
    if !read.is_null() {
        // SAFETY: the caller's.
        unsafe { *read = n as u32 };
    }
    1
}

/// Fake device handles start here (no real handle is this high and even).
pub const FAKE: usize = 0x7f00_0000_0000;

fn driver() -> MutexGuard<'static, Driver> {
    static DRIVER: OnceLock<Mutex<Driver>> = OnceLock::new();
    DRIVER
        .get_or_init(|| {
            Mutex::new(Driver {
                record: Record::default(),
                open: Box::new(|_| Err(2)),
                reply: Box::new(|_, _| 0xc000_0010_u32 as i32),
                fs: Fs::default(),
            })
        })
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

unsafe extern "system" fn fake_create_file_w(
    path: *const u16,
    access: u32,
    share: u32,
    _: *const c_void,
    disposition: u32,
    flags: u32,
    _: Handle,
) -> Handle {
    // SAFETY: a NUL-terminated path.
    let name = unsafe { super::wide_str(path) };
    let mut d = driver();
    d.record.opened.push(format!("{name} access {access:#x} share {share} disposition {disposition} flags {flags:#x}"));
    match (d.open)(&name) {
        Ok(h) => h as Handle,
        Err(e) => {
            // SAFETY: plain.
            unsafe { windows_sys::Win32::Foundation::SetLastError(e) };
            -1isize as Handle
        }
    }
}

fn answer(handle: Handle, code: u32, input: *const c_void, input_len: u32, output: *mut c_void, output_len: u32) -> i32 {
    let bytes = if input.is_null() {
        Vec::new()
    } else {
        // SAFETY: the caller's buffers.
        unsafe { std::slice::from_raw_parts(input.cast::<u8>(), input_len as usize) }.to_vec()
    };
    let call = Call { handle: handle as usize, code, input: bytes.clone() };
    let mut d = driver();
    d.record.calls.push(call.clone());
    let mut buffer = bytes;
    buffer.resize(input_len.max(output_len) as usize, 0);
    let status = (d.reply)(&call, &mut buffer);
    if !output.is_null() {
        // SAFETY: the caller's output buffer.
        unsafe { std::ptr::copy_nonoverlapping(buffer.as_ptr(), output.cast(), output_len as usize) };
    }
    status
}

unsafe extern "system" fn fake_nt_device_io_control_file(
    file: Handle,
    _event: Handle,
    _: *const c_void,
    _: *const c_void,
    io: *mut IoStatus,
    code: u32,
    input: *const c_void,
    input_len: u32,
    output: *mut c_void,
    output_len: u32,
) -> i32 {
    let status = answer(file, code, input, input_len, output, output_len);
    if !io.is_null() {
        // SAFETY: the caller's status block.
        unsafe { io.write(IoStatus { status: status as u32 as usize, information: output_len as usize }) };
    }
    status
}

unsafe extern "system" fn fake_device_io_control(
    device: Handle,
    code: u32,
    input: *const c_void,
    input_len: u32,
    output: *mut c_void,
    output_len: u32,
    returned: *mut u32,
    _: *mut c_void,
) -> i32 {
    let status = answer(device, code, input, input_len, output, output_len);
    if !returned.is_null() {
        // SAFETY: the caller's.
        unsafe { *returned = output_len };
    }
    if status < 0 {
        // SAFETY: plain.
        unsafe { windows_sys::Win32::Foundation::SetLastError(sys::dos_error(status)) };
        0
    } else {
        1
    }
}

/// Volume queries reach the driver's answer as code `0xffff_0000 | class`,
/// no input, the buffer to fill. A failure also sets the thread's last
/// error (the library reads it), so both sides see the same one.
pub const VOLUME_QUERY: u32 = 0xffff_0000;

unsafe extern "system" fn fake_query_volume(file: Handle, io: *mut IoStatus, buffer: *mut c_void, length: u32, class: u32) -> i32 {
    let status = answer(file, VOLUME_QUERY | class, std::ptr::null(), 0, buffer, length);
    if !io.is_null() {
        // SAFETY: the caller's status block.
        unsafe { io.write(IoStatus { status: status as u32 as usize, information: length as usize }) };
    }
    if status < 0 {
        // SAFETY: plain.
        unsafe { windows_sys::Win32::Foundation::SetLastError(sys::dos_error(status)) };
    }
    status
}

unsafe extern "system" fn fake_close_handle(handle: Handle) -> i32 {
    if (handle as usize) >= FAKE {
        driver().record.closed.push(handle as usize);
        return 1;
    }
    // SAFETY: a real handle of the caller's.
    unsafe { windows_sys::Win32::Foundation::CloseHandle(handle) }
}

unsafe extern "system" fn fake_sleep(ms: u32) {
    driver().record.sleeps.push(ms);
}

static TICK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1_000_000);
static STEP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The clock: still unless a test sets it moving ([`Session::ticks`]).
unsafe extern "system" fn fake_tick() -> u64 {
    TICK.fetch_add(STEP.load(std::sync::atomic::Ordering::Relaxed), std::sync::atomic::Ordering::Relaxed)
}

/// WNF, faked: the state name each call names, the last subscription's
/// handler and context (a test fires it), and statuses to answer with.
pub struct Wnf {
    pub names: Vec<u64>,
    pub handler: usize,
    pub context: usize,
    pub query: i32,
    pub subscribe: i32,
    pub unsubscribed: Vec<usize>,
}

pub static WNF: Mutex<Wnf> = Mutex::new(Wnf { names: Vec::new(), handler: 0, context: 0, query: 0, subscribe: 0, unsubscribed: Vec::new() });

unsafe extern "system" fn fake_query_wnf(stamp: *mut u32, name: u64, _: *const c_void, _: *mut c_void, _: *const c_void) -> i32 {
    let mut w = WNF.lock().unwrap_or_else(|e| e.into_inner());
    w.names.push(name);
    // SAFETY: the caller's stamp.
    unsafe { *stamp = 0x55 };
    w.query
}

unsafe extern "system" fn fake_subscribe_wnf(out: *mut *mut c_void, name: u64, stamp: u32, handler: *const c_void, context: *mut c_void, _: *const c_void, _: u32, _: u32) -> i32 {
    let mut w = WNF.lock().unwrap_or_else(|e| e.into_inner());
    w.names.push(name ^ u64::from(stamp));
    w.handler = handler as usize;
    w.context = context as usize;
    if w.subscribe >= 0 {
        // SAFETY: the caller's out.
        unsafe { *out = 0x5_0000 as *mut c_void };
    }
    w.subscribe
}

unsafe extern "system" fn fake_unsubscribe_wnf(subscription: *mut c_void) -> i32 {
    WNF.lock().unwrap_or_else(|e| e.into_inner()).unsubscribed.push(subscription as usize);
    0
}

/// The console is not a server (a title's console), for the reference as
/// for the port.
unsafe extern "system" fn fake_server_type() -> u32 {
    0
}

unsafe extern "C" fn fake_putws(text: *const u16) -> i32 {
    // SAFETY: a NUL-terminated line.
    let line = unsafe { super::wide_str(text) };
    driver().record.messages.push(line);
    0
}

const FAKE_SYS: Sys = Sys {
    create_file_w: fake_create_file_w,
    nt_device_io_control_file: fake_nt_device_io_control_file,
    device_io_control: fake_device_io_control,
    close_handle: fake_close_handle,
    sleep: fake_sleep,
    get_tick_count64: fake_tick,
    putws: fake_putws,
    rtl_query_wnf_state_data: fake_query_wnf,
    rtl_subscribe_wnf: fake_subscribe_wnf,
    rtl_unsubscribe_wnf: fake_unsubscribe_wnf,
    nt_query_volume_information_file: fake_query_volume,
    get_drive_type_w: fake_drive_type,
    find_first_file_w: fake_find_first,
    find_next_file_w: fake_find_next,
    find_close: fake_find_close,
    set_file_pointer_ex: fake_seek,
    read_file: fake_read,
    ..sys::HOST
};

/// The loaded reference, its imports patched.
struct Library {
    base: usize,
}

// SAFETY: a module handle.
unsafe impl Send for Library {}
unsafe impl Sync for Library {}

fn library() -> Option<&'static Library> {
    static LIBRARY: OnceLock<Option<Library>> = OnceLock::new();
    LIBRARY.get_or_init(load).as_ref()
}

fn load() -> Option<Library> {
    use windows_sys::Win32::System::LibraryLoader::{LOAD_WITH_ALTERED_SEARCH_PATH, LoadLibraryExW};
    let dir = std::env::var_os("VICTORY_REFERENCE_DIR")?;
    let path = std::path::Path::new(&dir).join("xcrdapi.dll");
    let wide: Vec<u16> = std::os::windows::ffi::OsStrExt::encode_wide(path.as_os_str()).chain([0]).collect();
    // SAFETY: a NUL-terminated path.
    let module = unsafe { LoadLibraryExW(wide.as_ptr(), std::ptr::null_mut(), LOAD_WITH_ALTERED_SEARCH_PATH) };
    if module.is_null() {
        eprintln!("reference: {} did not load ({})", path.display(), sys::last_error());
        return None;
    }
    let base = module as usize;
    // SAFETY: a module the loader just mapped.
    unsafe { patch_imports(base) };
    Some(Library { base })
}

/// Points the module's imports we fake at the fakes.
///
/// # Safety
/// `base` is a loaded PE module.
unsafe fn patch_imports(base: usize) {
    use windows_sys::Win32::System::Memory::{PAGE_READWRITE, VirtualProtect};
    let fakes: [(&str, usize); 18] = [
        ("GetDriveTypeW", fake_drive_type as *const () as usize),
        ("FindFirstFileW", fake_find_first as *const () as usize),
        ("FindNextFileW", fake_find_next as *const () as usize),
        ("FindClose", fake_find_close as *const () as usize),
        ("SetFilePointerEx", fake_seek as *const () as usize),
        ("ReadFile", fake_read as *const () as usize),
        ("NtQueryVolumeInformationFile", fake_query_volume as *const () as usize),
        ("RtlQueryWnfStateData", fake_query_wnf as *const () as usize),
        ("RtlSubscribeWnfStateChangeNotification", fake_subscribe_wnf as *const () as usize),
        ("RtlUnsubscribeWnfNotificationWaitForCompletion", fake_unsubscribe_wnf as *const () as usize),
        ("XpalGetServerType", fake_server_type as *const () as usize),
        ("CreateFileW", fake_create_file_w as *const () as usize),
        ("NtDeviceIoControlFile", fake_nt_device_io_control_file as *const () as usize),
        ("DeviceIoControl", fake_device_io_control as *const () as usize),
        ("CloseHandle", fake_close_handle as *const () as usize),
        ("Sleep", fake_sleep as *const () as usize),
        ("GetTickCount64", fake_tick as *const () as usize),
        ("_putws", fake_putws as *const () as usize),
    ];
    // SAFETY: reading the module's own headers.
    unsafe {
        let read32 = |at: usize| (at as *const u32).read_unaligned();
        let pe = base + read32(base + 0x3c) as usize;
        let imports = base + read32(pe + 24 + 112 + 8) as usize;
        let mut desc = imports;
        while read32(desc) != 0 || read32(desc + 12) != 0 {
            let names = base + read32(desc) as usize;
            let iat = base + read32(desc + 16) as usize;
            let mut i = 0;
            loop {
                let entry = (names as *const u64).add(i).read();
                if entry == 0 {
                    break;
                }
                if entry & (1 << 63) == 0 {
                    let name = std::ffi::CStr::from_ptr((base + entry as usize + 2) as *const i8);
                    if let Some(&(_, fake)) = fakes.iter().find(|(n, _)| name.to_bytes() == n.as_bytes()) {
                        let slot = (iat as *mut usize).add(i);
                        let mut old = 0;
                        VirtualProtect(slot.cast(), 8, PAGE_READWRITE, &mut old);
                        slot.write(fake);
                        VirtualProtect(slot.cast(), 8, old, &mut old);
                    }
                }
                i += 1;
            }
            desc += 20;
        }
    }
}

/// An export of the reference, if it is loaded.
pub fn reference(name: &str) -> Option<usize> {
    let library = library()?;
    let name = std::ffi::CString::new(name).ok()?;
    // SAFETY: the module and a NUL-terminated name.
    let address = unsafe { windows_sys::Win32::System::LibraryLoader::GetProcAddress(library.base as _, name.as_ptr().cast()) }?;
    Some(address as usize)
}

/// The layer's export of that name.
pub fn layer(name: &str) -> usize {
    super::exports()
        .into_iter()
        .chain(crate::modules::dstorage::module().exports)
        .find(|e| e.name == name)
        .unwrap_or_else(|| panic!("the layer has no {name}"))
        .address
}

/// A test's hold on the fake driver: one at a time, everything reset, the
/// port pointed at the fake.
pub struct Session {
    _lock: MutexGuard<'static, ()>,
}

pub fn session() -> Session {
    static LOCK: Mutex<()> = Mutex::new(());
    let lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    sys::set(FAKE_SYS);
    let _ = library();
    {
        let mut d = driver();
        d.record = Record::default();
        d.open = Box::new(|_| Err(2));
        d.reply = Box::new(|_, _| 0xc000_0010_u32 as i32);
        d.fs = Fs::default();
    }
    STEP.store(0, std::sync::atomic::Ordering::Relaxed);
    TICK.store(1_000_000, std::sync::atomic::Ordering::Relaxed);
    Session { _lock: lock }
}

impl Session {
    /// How CreateFileW answers.
    pub fn open(&self, open: impl FnMut(&str) -> Result<usize, u32> + Send + 'static) {
        driver().open = Box::new(open);
    }

    /// How the driver answers.
    pub fn reply(&self, reply: impl FnMut(&Call, &mut [u8]) -> i32 + Send + 'static) {
        driver().reply = Box::new(reply);
    }

    /// How the fake file system looks (its cursors reset).
    pub fn fs(&self, edit: impl FnOnce(&mut Fs)) {
        let mut d = driver();
        d.fs = Fs { next: 0, position: 0, ..d.fs.clone() };
        edit(&mut d.fs);
    }

    /// The clock restarts, advancing `step` ms each time it is read.
    pub fn ticks(&self, step: u64) {
        TICK.store(1_000_000, std::sync::atomic::Ordering::Relaxed);
        STEP.store(step, std::sync::atomic::Ordering::Relaxed);
    }

    /// What was seen since the last take, and a clean slate.
    pub fn take(&self) -> Record {
        std::mem::take(&mut driver().record)
    }
}

/// A driver that accepts every block: return code 0, the body as sent;
/// `edit` may fill in what the driver would hand back.
pub fn accepting(mut edit: impl FnMut(u32, &mut [u8]) + Send + 'static) -> impl FnMut(&Call, &mut [u8]) -> i32 + Send + 'static {
    move |call, buffer| {
        if buffer.len() >= 0x1c {
            buffer[0x14..0x18].fill(0);
            let code = u32::from_le_bytes(buffer[0x10..0x14].try_into().unwrap());
            if code == 0x7800_000c {
                buffer[4..12].copy_from_slice(&super::srb::SIGNATURE);
            }
            edit(code, &mut buffer[0x1c..]);
        }
        let _ = call;
        0
    }
}
