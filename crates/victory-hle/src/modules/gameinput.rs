//! `gameinput.dll`: input devices from the host. Gamepads come from XInput
//! (host slots 0-3); while none is connected, the keyboard drives one
//! stand-in gamepad (`FALLBACK_BUTTONS`). The keyboard is a device of its own
//! too. A poll thread turns host state into readings (a new one per change)
//! and runs device callbacks on connect and disconnect.
#![allow(non_snake_case)]
// Title code calls these through the objects, passing pointers the real API
// would trust.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::collections::VecDeque;
use std::ffi::c_void;
use std::ops::Deref;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Condvar, LazyLock, Mutex, MutexGuard, PoisonError};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use super::xasync::{E_INVALIDARG, E_NOTIMPL};
use super::xgameruntime::{Guid, HRESULT, NOT_SUPPORTED, S_OK};
use crate::{HleExport, HleModule};

/// The documented errors.
pub const DEVICE_DISCONNECTED: HRESULT = 0x838A_0001_u32 as i32;
pub const DEVICE_NOT_FOUND: HRESULT = 0x838A_0002_u32 as i32;
pub const READING_NOT_FOUND: HRESULT = 0x838A_0003_u32 as i32;
pub const REFERENCE_TOO_OLD: HRESULT = 0x838A_0004_u32 as i32;
const E_NOINTERFACE: HRESULT = 0x8000_4002_u32 as i32;
/// A motor index past the device's motors (console `GameInput.dll`).
const E_BOUNDS: HRESULT = 0x8000_000B_u32 as i32;

/// Input kinds (bits) our devices have.
const KEYBOARD_KIND: u32 = 0x10;
const GAMEPAD_KIND: u32 = 0x4_0000;
/// Device status of a connected device: connected, input enabled.
const STATUS_CONNECTED: u32 = 0x3;
const ASYNC_ENUMERATION: u32 = 1;

/// Gamepad buttons; the title maps each bit to one of its pad buttons.
const MENU: u32 = 0x1;
const VIEW: u32 = 0x2;
const A: u32 = 0x4;
const B: u32 = 0x8;
const X: u32 = 0x10;
const Y: u32 = 0x20;
const UP: u32 = 0x40;
const DOWN: u32 = 0x80;
const LEFT: u32 = 0x100;
const RIGHT: u32 = 0x200;
const LEFT_SHOULDER: u32 = 0x400;
const RIGHT_SHOULDER: u32 = 0x800;
const LEFT_STICK: u32 = 0x1000;
const RIGHT_STICK: u32 = 0x2000;

/// Devices: host pad slots, the keyboard's stand-in pad, the keyboard.
const PADS: usize = 4;
const FALLBACK: usize = PADS;
const KEYBOARD: usize = PADS + 1;
const DEVICES: usize = PADS + 2;

/// Readings kept per device for next/previous lookups.
const HISTORY: usize = 256;
const POLL: Duration = Duration::from_millis(8);

/// The keyboard's stand-in gamepad (virtual key, button); README lists it.
/// Sticks: W/A/S/D left, I/J/K/L right; triggers Z and C.
const FALLBACK_BUTTONS: [(u8, u32); 14] = [
    (0x1B, MENU), // Esc
    (0x09, VIEW), // Tab
    (0x0D, A),    // Enter
    (0x08, B),    // Backspace
    (b'X', X),
    (b'Y', Y),
    (0x26, UP), // arrows
    (0x28, DOWN),
    (0x25, LEFT),
    (0x27, RIGHT),
    (b'Q', LEFT_SHOULDER),
    (b'E', RIGHT_SHOULDER),
    (b'F', LEFT_STICK),
    (b'H', RIGHT_STICK),
];

/// A gamepad reading: triggers 0 to 1, sticks -1 to 1 (up positive).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct PadState {
    buttons: u32,
    left_trigger: f32,
    right_trigger: f32,
    left_x: f32,
    left_y: f32,
    right_x: f32,
    right_y: f32,
}

/// One key held down: scan code (0xE0xx when extended), character, key.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct KeyState {
    scan_code: u32,
    code_point: u32,
    virtual_key: u8,
    dead_key: bool,
}

const _: () = assert!(size_of::<KeyState>() == 12 && size_of::<PadState>() == 28);

/// Host input, or a fake in tests.
trait Source: Send {
    /// The pad in host slot `slot`, `None` when the slot is empty.
    fn pad(&mut self, slot: usize) -> Option<PadState>;
    /// Keys held down; none while the title's window lacks focus.
    fn keys(&mut self) -> Vec<KeyState>;
    /// Motor speeds: low frequency (left), high frequency (right).
    fn rumble(&mut self, slot: usize, low: u16, high: u16);
}

fn fallback(keys: &[KeyState]) -> PadState {
    let on = |vk: u8| {
        if keys.iter().any(|k| k.virtual_key == vk) {
            1.0
        } else {
            0.0
        }
    };
    PadState {
        buttons: FALLBACK_BUTTONS
            .iter()
            .filter(|(vk, _)| on(*vk) != 0.0)
            .fold(0, |all, (_, bit)| all | bit),
        left_trigger: on(b'Z'),
        right_trigger: on(b'C'),
        left_x: on(b'D') - on(b'A'),
        left_y: on(b'W') - on(b'S'),
        right_x: on(b'L') - on(b'J'),
        right_y: on(b'I') - on(b'K'),
    }
}

/// Two pads as one: buttons of both, and of each axis and trigger the one
/// pushed further.
fn merged(a: PadState, b: PadState) -> PadState {
    let further = |x: f32, y: f32| if y.abs() > x.abs() { y } else { x };
    PadState {
        buttons: a.buttons | b.buttons,
        left_trigger: further(a.left_trigger, b.left_trigger),
        right_trigger: further(a.right_trigger, b.right_trigger),
        left_x: further(a.left_x, b.left_x),
        left_y: further(a.left_y, b.left_y),
        right_x: further(a.right_x, b.right_x),
        right_y: further(a.right_y, b.right_y),
    }
}

/// Whether the title sees one pad that is always there, fed by the keyboard
/// and by every host pad (on by default; `VICTORY_ONE_PAD=0` shows each host
/// pad as a device of its own, and the stand-in only while there is none).
/// The title keeps to the pad it started with: with the stand-in taken away
/// when a pad was switched on, it showed its controller-disconnected screen
/// until the pad was off again. It also keeps a pad the host lists twice (its
/// own slot and a remapper's) from being two players.
fn one_pad() -> bool {
    std::env::var_os("VICTORY_ONE_PAD").is_none_or(|v| v != "0")
}

#[repr(C)]
struct Table<const N: usize>([*const (); N]);
// SAFETY: function addresses only.
unsafe impl<const N: usize> Sync for Table<N> {}

/// The input object; one per hub.
#[repr(C)]
pub struct Object {
    vtable: &'static Table<22>,
    hub: *const Hub,
    /// References the title holds: the console object starts at 0 and
    /// each `GameInputCreate` adds one (RVA 0x1e20, 0x5560).
    refs: AtomicU32,
}

#[repr(C)]
pub struct Device {
    vtable: &'static Table<22>,
    hub: *const Hub,
    index: usize,
    info: DeviceInfo,
}

/// What a device says about itself. The title reads `id` (+0x20); the rest
/// follows the public layout and is zero where we have nothing to say.
#[repr(C)]
#[derive(Default)]
#[allow(dead_code)] // read by title code
struct DeviceInfo {
    size: u32,
    vendor: u16,
    product: u16,
    revision: u16,
    interface: u8,
    collection: u8,
    usage: [u16; 2],
    versions: [u16; 8],
    id: [u8; 32],
    root_id: [u8; 32],
    family: i32,
    capabilities: u32,
    supported_input: u32,
    rumble_motors: u32,
    /// Counts and optional detail pointers.
    rest: [u64; 26],
}

const _: () = assert!(std::mem::offset_of!(DeviceInfo, id) == 0x20);

fn kind(index: usize) -> u32 {
    if index == KEYBOARD {
        KEYBOARD_KIND
    } else {
        GAMEPAD_KIND
    }
}

impl Device {
    fn new(index: usize) -> Device {
        let mut id = [0; 32];
        id[..7].copy_from_slice(b"victory");
        id[31] = index as u8 + 1;
        let info = DeviceInfo {
            size: size_of::<DeviceInfo>() as u32,
            id,
            root_id: id,
            supported_input: kind(index),
            // Low and high frequency.
            rumble_motors: if index <= FALLBACK { 0x3 } else { 0 },
            ..Default::default()
        };
        Device {
            vtable: &DEVICE,
            hub: std::ptr::null(),
            index,
            info,
        }
    }

    fn hub(&self) -> &'static Hub {
        // SAFETY: set by `Hub::new`; hubs are never freed.
        unsafe { &*self.hub }
    }
}

#[derive(PartialEq)]
enum Value {
    Pad(PadState),
    Keys(Vec<KeyState>),
}

#[repr(C)]
pub struct Reading {
    vtable: &'static Table<25>,
    refs: AtomicU32,
    device: *const Device,
    sequence: u64,
    timestamp: u64,
    value: Value,
}

impl Reading {
    fn kind(&self) -> u32 {
        match self.value {
            Value::Pad(_) => GAMEPAD_KIND,
            Value::Keys(_) => KEYBOARD_KIND,
        }
    }
}

/// A counted reference to a reading.
struct Ref(NonNull<Reading>);

impl Ref {
    fn new(reading: Reading) -> Ref {
        Ref(NonNull::from(Box::leak(Box::new(reading))))
    }

    /// Another reference, for the title.
    fn share(&self) -> *const Reading {
        reading_add_ref(self.0.as_ptr());
        self.0.as_ptr()
    }
}

impl Deref for Ref {
    type Target = Reading;
    fn deref(&self) -> &Reading {
        // SAFETY: we hold a reference, so it is alive.
        unsafe { self.0.as_ref() }
    }
}

impl Drop for Ref {
    fn drop(&mut self) {
        reading_release(self.0.as_ptr());
    }
}

type DeviceCallback = extern "system" fn(u64, *mut c_void, *const Device, u64, u32, u32);

#[derive(Clone, Copy)]
struct Registration {
    token: u64,
    /// One device's address, or 0 for any.
    device: usize,
    kind: u32,
    status: u32,
    context: usize,
    callback: DeviceCallback,
}

impl Registration {
    fn matches(&self, devices: &[Device; DEVICES], index: usize) -> bool {
        kind(index) & self.kind != 0
            && (self.device == 0 || self.device == &devices[index] as *const Device as usize)
    }

    fn event(&self, device: usize, timestamp: u64, current: u32, previous: u32) -> Event {
        Event {
            registration: *self,
            device,
            timestamp,
            current,
            previous,
        }
    }
}

struct Event {
    registration: Registration,
    device: usize,
    timestamp: u64,
    current: u32,
    previous: u32,
}

#[derive(Default)]
struct Slot {
    connected: bool,
    sequence: u64,
    history: VecDeque<Ref>,
}

struct State {
    slots: [Slot; DEVICES],
    callbacks: Vec<Registration>,
    /// Asynchronous enumerations, delivered by the next poll.
    pending: Vec<Event>,
    /// Device callbacks running now: token and thread.
    running: Vec<(u64, ThreadId)>,
}

pub struct Hub {
    object: Object,
    devices: [Device; DEVICES],
    /// Apart from the state: XInput can be slow, and the title reads
    /// readings every frame.
    source: Mutex<Box<dyn Source>>,
    /// The title sees one pad ([`one_pad`]).
    one_pad: bool,
    state: Mutex<State>,
    /// A device callback returned.
    returned: Condvar,
}

fn lock<T: ?Sized>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// SAFETY: the back pointers are set before the hub is shared and never
// change; what changes sits behind the mutex or is atomic.
unsafe impl Send for Hub {}
// SAFETY: as above.
unsafe impl Sync for Hub {}

/// Microseconds, the clock of every timestamp we hand out: as the console's
/// GameInput.dll (`GetCurrentTimestamp` at RVA 0x4e70 -> 0xb418), the
/// performance counter scaled to microseconds, so readings line up with
/// the title's own `QueryPerformanceCounter` times.
#[cfg(windows)]
fn now() -> u64 {
    use windows_sys::Win32::System::Performance::{
        QueryPerformanceCounter, QueryPerformanceFrequency,
    };
    static FREQUENCY: LazyLock<u64> = LazyLock::new(|| {
        let mut f = 0;
        // SAFETY: writes one integer.
        unsafe { QueryPerformanceFrequency(&mut f) };
        (f as u64).max(1)
    });
    let mut ticks = 0;
    // SAFETY: writes one integer.
    unsafe { QueryPerformanceCounter(&mut ticks) };
    (u128::from(ticks as u64) * 1_000_000 / u128::from(*FREQUENCY)) as u64
}

#[cfg(not(windows))]
fn now() -> u64 {
    static START: LazyLock<Instant> = LazyLock::new(Instant::now);
    START.elapsed().as_micros() as u64
}

impl Hub {
    fn new(source: Box<dyn Source>, one_pad: bool) -> &'static Hub {
        let hub = Box::into_raw(Box::new(Hub {
            object: Object {
                vtable: &INPUT,
                hub: std::ptr::null(),
                refs: AtomicU32::new(0),
            },
            devices: std::array::from_fn(Device::new),
            source: Mutex::new(source),
            one_pad,
            state: Mutex::new(State {
                slots: Default::default(),
                callbacks: Vec::new(),
                pending: Vec::new(),
                running: Vec::new(),
            }),
            returned: Condvar::new(),
        }));
        // SAFETY: just allocated and never freed; nothing else sees it yet.
        unsafe {
            (*hub).object.hub = hub;
            for device in &mut (*hub).devices {
                device.hub = hub;
            }
            &*hub
        }
    }

    /// A hub over the host's input, polled from now on.
    fn start(source: Box<dyn Source>) -> &'static Hub {
        let hub = Hub::new(source, one_pad());
        hub.poll();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(POLL);
                hub.poll();
            }
        });
        hub
    }

    fn state(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    /// Reads the host once; device callbacks run on the calling thread.
    fn poll(&self) {
        let (pads, keys) = {
            let mut source = lock(&self.source);
            let pads: [_; PADS] = std::array::from_fn(|slot| source.pad(slot));
            (pads, source.keys())
        };
        let events = self.state().poll(&self.devices, self.one_pad, pads, keys);
        self.deliver(events);
    }

    /// Runs callbacks, never under our lock: they call back into us. One
    /// unregistered since the event was taken does not run, and
    /// `UnregisterCallback` waits for one that runs.
    fn deliver(&self, events: Vec<Event>) {
        let me = std::thread::current().id();
        for e in events {
            let r = e.registration;
            {
                let mut state = self.state();
                if !state.callbacks.iter().any(|c| c.token == r.token) {
                    continue;
                }
                state.running.push((r.token, me));
            }
            (r.callback)(
                r.token,
                r.context as *mut c_void,
                &self.devices[e.device],
                e.timestamp,
                e.current,
                e.previous,
            );
            let mut state = self.state();
            if let Some(i) = state.running.iter().position(|&x| x == (r.token, me)) {
                state.running.swap_remove(i);
            }
            self.returned.notify_all();
        }
    }

    fn index(&self, device: *const Device) -> Option<usize> {
        self.devices.iter().position(|d| std::ptr::eq(d, device))
    }
}

impl State {
    fn poll(
        &mut self,
        devices: &[Device; DEVICES],
        one_pad: bool,
        pads: [Option<PadState>; PADS],
        keys: Vec<KeyState>,
    ) -> Vec<Event> {
        let now = now();
        let mut events = std::mem::take(&mut self.pending);
        if one_pad {
            let pad = pads.into_iter().flatten().fold(fallback(&keys), merged);
            self.update(devices, FALLBACK, Some(Value::Pad(pad)), now, &mut events);
            self.update(devices, KEYBOARD, Some(Value::Keys(keys)), now, &mut events);
            return events;
        }
        let any_pad = pads.iter().any(Option::is_some);
        for (slot, pad) in pads.into_iter().enumerate() {
            self.update(devices, slot, pad.map(Value::Pad), now, &mut events);
        }
        let fallback = (!any_pad).then(|| Value::Pad(fallback(&keys)));
        self.update(devices, FALLBACK, fallback, now, &mut events);
        self.update(devices, KEYBOARD, Some(Value::Keys(keys)), now, &mut events);
        events
    }

    /// Records a device's state: a reading when it changed, callbacks when
    /// the device came or went.
    fn update(
        &mut self,
        devices: &[Device; DEVICES],
        index: usize,
        value: Option<Value>,
        now: u64,
        events: &mut Vec<Event>,
    ) {
        let slot = &mut self.slots[index];
        let was = slot.connected;
        slot.connected = value.is_some();
        if let Some(value) = value
            && (!was || slot.history.back().is_none_or(|r| r.value != value))
        {
            slot.sequence += 1;
            if slot.history.len() == HISTORY {
                slot.history.pop_front();
            }
            slot.history.push_back(Ref::new(Reading {
                vtable: &READING,
                refs: AtomicU32::new(1),
                device: &devices[index],
                sequence: slot.sequence,
                timestamp: now,
                value,
            }));
        }
        if was != slot.connected {
            tracing::info!(device = index, connected = !was, "input device");
            let (current, previous) = if was {
                (0, STATUS_CONNECTED)
            } else {
                (STATUS_CONNECTED, 0)
            };
            events.extend(
                self.callbacks
                    .iter()
                    .filter(|r| r.matches(devices, index) && r.status & STATUS_CONNECTED != 0)
                    .map(|r| r.event(index, now, current, previous)),
            );
        }
    }
}

#[cfg(windows)]
fn host() -> Box<dyn Source> {
    Box::new(host::Host::default())
}

#[cfg(not(windows))]
fn host() -> Box<dyn Source> {
    struct Nothing;
    impl Source for Nothing {
        fn pad(&mut self, _: usize) -> Option<PadState> {
            None
        }
        fn keys(&mut self) -> Vec<KeyState> {
            Vec::new()
        }
        fn rumble(&mut self, _: usize, _: u16, _: u16) {}
    }
    Box::new(Nothing)
}

static HUB: LazyLock<&'static Hub> = LazyLock::new(|| Hub::start(host()));

type This = *const Object;
type Out = *mut *const c_void;

/// The DLL's only export (ordinal 1): the input object, static and shared.
pub extern "system" fn GameInputCreate(out: *mut This) -> HRESULT {
    tracing::info!("GameInputCreate");
    if out.is_null() {
        return E_INVALIDARG;
    }
    HUB.object.refs.fetch_add(1, Ordering::AcqRel);
    // SAFETY: checked non-null; the title passes room for a pointer.
    unsafe { out.write(&HUB.object) };
    S_OK
}

fn hub(this: This) -> &'static Hub {
    // SAFETY: `this` is a hub's object; hubs are never freed.
    unsafe { &*(*this).hub }
}

/// Clears a COM out pointer and returns `hr`.
fn fail(out: Out, hr: HRESULT) -> HRESULT {
    if !out.is_null() {
        // SAFETY: checked non-null; the title passes room for a pointer.
        unsafe { out.write(std::ptr::null()) };
    }
    hr
}

/// Gives the title a reference to `reading`, or fails as with none.
fn hand(out: Out, reading: Option<&Ref>) -> HRESULT {
    match reading {
        _ if out.is_null() => E_INVALIDARG,
        Some(reading) => {
            // SAFETY: checked non-null; the title passes room for a pointer.
            unsafe { out.write(reading.share().cast()) };
            S_OK
        }
        None => fail(out, READING_NOT_FOUND),
    }
}

const fn iid(data1: u32, data2: u16, data3: u16, data4: [u8; 8]) -> Guid {
    Guid::new(data1, data2, data3, data4)
}
const IUNKNOWN: Guid = iid(0, 0, 0, [0xc0, 0, 0, 0, 0, 0, 0, 0x46]);
const IAGILE_OBJECT: Guid = iid(
    0x94ea2b94,
    0xe9cc,
    0x49e0,
    [0xc0, 0xff, 0xee, 0x64, 0xca, 0x8f, 0x5b, 0x90],
);
const INO_MARSHAL: Guid = iid(
    0xecc8691b,
    0xc1db,
    0x4dc0,
    [0x85, 0x5e, 0x65, 0xf6, 0xc5, 0x51, 0xaf, 0x49],
);
/// The interface ids each object answers, from the console's DLL: the
/// input object (QI at RVA 0x7b60), devices (0x7c40), readings (0x7fc0).
/// Each also answers IAgileObject, INoMarshal and IUnknown.
const INPUT_IIDS: [Guid; 2] = [
    iid(
        0x11be2a7e,
        0x4254,
        0x445a,
        [0x9c, 0x09, 0xff, 0xc4, 0x0f, 0x00, 0x69, 0x18],
    ),
    iid(
        0x1ff158d4,
        0x2a34,
        0x4dc7,
        [0xba, 0xc4, 0x9b, 0x71, 0x1f, 0x3a, 0xfb, 0x59],
    ),
];
const DEVICE_IIDS: [Guid; 2] = [
    iid(
        0x31dd86fb,
        0x4c1b,
        0x408a,
        [0x86, 0x8f, 0x43, 0x9b, 0x3c, 0xd4, 0x71, 0x25],
    ),
    iid(
        0x3f57311f,
        0x7175,
        0x4d26,
        [0xa1, 0x00, 0xf8, 0x3c, 0x2c, 0x7b, 0x96, 0x2c],
    ),
];
const READING_IIDS: [Guid; 2] = [
    iid(
        0x2156947a,
        0xe1fa,
        0x4de0,
        [0xa3, 0x0b, 0xd8, 0x12, 0x93, 0x1d, 0xbd, 0x8d],
    ),
    iid(
        0x2f7a85f7,
        0x90ea,
        0x4573,
        [0xad, 0x83, 0xe5, 0x82, 0x15, 0x6a, 0x02, 0x9f],
    ),
];
const E_POINTER: HRESULT = 0x8000_4003_u32 as i32;

/// QueryInterface as the console's: a known id with no out pointer is
/// E_POINTER; a known id adds a reference and hands the object; any other
/// clears the out pointer and is E_NOINTERFACE.
fn query(
    this: *const c_void,
    iids: &[Guid],
    iid: *const Guid,
    out: Out,
    add_ref: impl FnOnce(),
) -> HRESULT {
    // SAFETY: the title passes an interface id (null is tolerated).
    let known = unsafe { iid.as_ref() }
        .is_some_and(|i| iids.contains(i) || [IAGILE_OBJECT, INO_MARSHAL, IUNKNOWN].contains(i));
    if !known {
        return fail(out, E_NOINTERFACE);
    }
    if out.is_null() {
        return E_POINTER;
    }
    add_ref();
    // SAFETY: checked non-null.
    unsafe { out.write(this) };
    S_OK
}
extern "system" fn input_query(this: This, iid: *const Guid, out: Out) -> HRESULT {
    query(this.cast(), &INPUT_IIDS, iid, out, || {
        input_add_ref(this);
    })
}
extern "system" fn device_query(this: *const Device, iid: *const Guid, out: Out) -> HRESULT {
    query(this.cast(), &DEVICE_IIDS, iid, out, || {})
}
/// The raw report interfaces (0x80f0) answer only for readings with a raw
/// report, which ours never have.
extern "system" fn reading_query(this: *const Reading, iid: *const Guid, out: Out) -> HRESULT {
    query(this.cast(), &READING_IIDS, iid, out, || {
        reading_add_ref(this);
    })
}
/// The input object's references (console AddRef 0x25c0, Release 0x8800:
/// the new count). Ours outlives a count of 0, and the next
/// `GameInputCreate` counts from 0 again as the console's new object would.
extern "system" fn input_add_ref(this: This) -> u32 {
    // SAFETY: `this` is a hub's object.
    unsafe { &(*this).refs }.fetch_add(1, Ordering::AcqRel) + 1
}
extern "system" fn input_release(this: This) -> u32 {
    // SAFETY: `this` is a hub's object.
    let refs = unsafe { &(*this).refs };
    let mut left = 0;
    let _ = refs.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
        left = n.saturating_sub(1);
        Some(left)
    });
    left
}
// shortcut: devices are not counted: the console's count includes
// references its service holds, which nothing here can match.
extern "system" fn add_ref(_: *const c_void) -> u32 {
    1
}

extern "system" fn timestamp(_: This) -> u64 {
    now()
}

extern "system" fn current_reading(
    this: This,
    kind: u32,
    device: *const Device,
    out: Out,
) -> HRESULT {
    let hub = hub(this);
    let state = hub.state();
    let newest = |i: usize| {
        state.slots[i]
            .history
            .back()
            .filter(|r| r.kind() & kind != 0)
    };
    let found = if device.is_null() {
        (0..DEVICES)
            .filter(|&i| state.slots[i].connected)
            .filter_map(newest)
            .max_by_key(|r| r.timestamp)
    } else {
        match hub.index(device) {
            None => return fail(out, E_INVALIDARG),
            Some(i) if !state.slots[i].connected => return fail(out, DEVICE_DISCONNECTED),
            Some(i) => newest(i),
        }
    };
    tracing::trace!(
        kind = format_args!("{kind:#x}"),
        device = ?hub.index(device),
        sequence = found.map(|r| r.sequence),
        "current reading"
    );
    hand(out, found)
}

/// The reading after (`forward`) or before `reference` on its device.
fn relative(
    this: This,
    reference: *const Reading,
    kind: u32,
    device: *const Device,
    out: Out,
    forward: bool,
) -> HRESULT {
    // As the console's GameInput.dll: the out pointer is cleared first; a
    // walk needs its device (none: E_NOTIMPL), one of ours, a reference
    // and the reference's device being that device (else E_INVALIDARG),
    // and the device connected.
    fail(out, S_OK);
    if device.is_null() {
        tracing::debug!(forward, "input reading walk without a device: E_NOTIMPL");
        return E_NOTIMPL;
    }
    let hub = hub(this);
    let Some(index) = hub.index(device) else {
        return E_INVALIDARG;
    };
    if reference.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: a reading we handed out and the title still holds.
    let reference = unsafe { &*reference };
    if !std::ptr::eq(device, reference.device) {
        return E_INVALIDARG;
    }
    let state = hub.state();
    if !state.slots[index].connected {
        return DEVICE_DISCONNECTED;
    }
    let history = &state.slots[index].history;
    if history
        .front()
        .is_none_or(|oldest| reference.sequence < oldest.sequence)
    {
        return fail(out, REFERENCE_TOO_OLD);
    }
    let wanted = |r: &&Ref| r.kind() & kind != 0;
    let found = if forward {
        history
            .iter()
            .filter(|r| r.sequence > reference.sequence)
            .find(wanted)
    } else {
        history
            .iter()
            .rev()
            .filter(|r| r.sequence < reference.sequence)
            .find(wanted)
    };
    hand(out, found)
}
extern "system" fn next_reading(
    this: This,
    reference: *const Reading,
    kind: u32,
    device: *const Device,
    out: Out,
) -> HRESULT {
    relative(this, reference, kind, device, out, true)
}
extern "system" fn previous_reading(
    this: This,
    reference: *const Reading,
    kind: u32,
    device: *const Device,
    out: Out,
) -> HRESULT {
    relative(this, reference, kind, device, out, false)
}
/// Not implemented by the console's GameInput.dll either: the out pointer
/// is cleared and the call fails with E_NOTIMPL.
extern "system" fn temporal_reading(
    _: This,
    _time: u64,
    _device: *const Device,
    out: Out,
) -> HRESULT {
    fail(out, E_NOTIMPL)
}

static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

/// Writes `value` through a token out pointer if it is non-null.
fn put_token(out: *mut u64, value: u64) {
    if !out.is_null() {
        // SAFETY: checked non-null.
        unsafe { out.write(value) };
    }
}

/// The console's GameInput.dll does not implement reading callbacks: the
/// token is cleared and the call fails with E_NOTIMPL.
extern "system" fn register_reading(
    _: This,
    _device: *const c_void,
    _kind: u32,
    _threshold: f32,
    _context: *mut c_void,
    _callback: *const c_void,
    out: *mut u64,
) -> HRESULT {
    put_token(out, 0);
    E_NOTIMPL
}

/// Device callbacks run on our poll thread when a device comes or goes.
/// Enumeration reports the devices already there: blocking on the caller's
/// thread before we return (the title registers holding a lock its callback
/// takes again), asynchronous on the next poll.
extern "system" fn register_device(
    this: This,
    device: *const Device,
    kind: u32,
    status: u32,
    enumeration: u32,
    context: *mut c_void,
    callback: Option<DeviceCallback>,
    out: *mut u64,
) -> HRESULT {
    put_token(out, 0);
    let Some(callback) = callback else {
        return E_INVALIDARG;
    };
    let hub = hub(this);
    let registration = Registration {
        token: NEXT_TOKEN.fetch_add(1, Ordering::Relaxed),
        device: device as usize,
        kind,
        status,
        context: context as usize,
        callback,
    };
    let mut found = Vec::new();
    {
        let mut state = hub.state();
        if enumeration != 0 && status & STATUS_CONNECTED != 0 {
            let now = now();
            found = (0..DEVICES)
                .filter(|&i| state.slots[i].connected && registration.matches(&hub.devices, i))
                .map(|i| registration.event(i, now, STATUS_CONNECTED, 0))
                .collect();
        }
        tracing::info!(
            kind = format_args!("{kind:#x}"),
            enumeration,
            devices = found.len(),
            "input device callback"
        );
        state.callbacks.push(registration);
        if enumeration == ASYNC_ENUMERATION {
            state.pending.append(&mut found);
        }
    }
    if !out.is_null() {
        // SAFETY: checked non-null.
        unsafe { out.write(registration.token) };
    }
    hub.deliver(found);
    S_OK
}
/// Guide button and keyboard layout callbacks: neither ever changes here.
/// As on the console the token is cleared first and a missing callback is
/// E_INVALIDARG.
extern "system" fn register_other(
    _: This,
    _device: *const c_void,
    _context: *mut c_void,
    callback: *const c_void,
    out: *mut u64,
) -> HRESULT {
    put_token(out, 0);
    if callback.is_null() {
        return E_INVALIDARG;
    }
    put_token(out, NEXT_TOKEN.fetch_add(1, Ordering::Relaxed));
    S_OK
}

/// Takes the registration away; whether there was one.
fn forget(this: This, token: u64) -> bool {
    let mut state = hub(this).state();
    let before = state.callbacks.len();
    state.callbacks.retain(|r| r.token != token);
    state.pending.retain(|e| e.registration.token != token);
    state.callbacks.len() != before
}
/// Token 0 is no callback; `u64::MAX` from inside a callback names that
/// callback (console GameInput.dll).
extern "system" fn stop_callback(this: This, token: u64) {
    let token = if token == u64::MAX {
        let me = std::thread::current().id();
        match hub(this).state().running.iter().find(|r| r.1 == me) {
            Some(r) => r.0,
            None => return,
        }
    } else {
        token
    };
    if token != 0 {
        forget(this, token);
    }
}
/// As the console's GameInput.dll: tokens 0 and `u64::MAX`, a call from
/// inside any device callback (which leaves the registration in place)
/// and an unknown token are false; otherwise the registration is taken
/// away and the result is true. A call of it running on another thread is
/// waited for, up to `timeout` µs.
extern "system" fn unregister_callback(this: This, token: u64, timeout: u64) -> bool {
    let hub = hub(this);
    let me = std::thread::current().id();
    if token == 0 || token == u64::MAX || hub.state().running.iter().any(|r| r.1 == me) {
        return false;
    }
    if !forget(this, token) {
        return false;
    }
    let deadline = Instant::now().checked_add(Duration::from_micros(timeout));
    let mut state = hub.state();
    loop {
        if !state.running.iter().any(|r| r.0 == token) {
            return true;
        }
        state = match deadline {
            None => hub
                .returned
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner),
            Some(deadline) => {
                let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                    // Taken away all the same: the console reports it so.
                    return true;
                };
                let waited = hub.returned.wait_timeout(state, left);
                waited.unwrap_or_else(PoisonError::into_inner).0
            }
        };
    }
}

/// Dispatcher and aggregate device: no title of ours asks for them.
extern "system" fn create_dispatcher(_: This, out: Out) -> HRESULT {
    tracing::warn!("input dispatcher not implemented");
    fail(out, E_NOTIMPL)
}
extern "system" fn create_aggregate(_: This, _kind: u32, out: Out) -> HRESULT {
    tracing::warn!("aggregate input device not implemented");
    fail(out, E_NOTIMPL)
}
extern "system" fn find_by_id(this: This, id: *const [u8; 32], out: Out) -> HRESULT {
    if id.is_null() || out.is_null() {
        return fail(out, E_INVALIDARG);
    }
    // SAFETY: checked non-null; the title passes a device id.
    let id = unsafe { &*id };
    match hub(this).devices.iter().find(|d| d.info.id == *id) {
        Some(device) => {
            // SAFETY: checked non-null.
            unsafe { out.write((device as *const Device).cast()) };
            S_OK
        }
        None => fail(out, DEVICE_NOT_FOUND),
    }
}
/// Lookup by object (console RVA 0x3f00): an object answering
/// IGameInputDevice is looked up by the id its info gives; the console's
/// other routes (Windows.Gaming.Input objects) find nothing here.
extern "system" fn find_by_object(this: This, object: *const *const usize, out: Out) -> HRESULT {
    type Query = extern "system" fn(*const c_void, *const Guid, Out) -> HRESULT;
    type Info = extern "system" fn(*const c_void) -> *const u8;
    type Release = extern "system" fn(*const c_void) -> u32;
    if object.is_null() {
        return fail(out, DEVICE_NOT_FOUND);
    }
    // SAFETY: a COM object: its first word is its method table, whose
    // slot 0 is QueryInterface.
    let query: Query = unsafe { std::mem::transmute(*(*object)) };
    let mut device = std::ptr::null();
    if query(object.cast(), &DEVICE_IIDS[0], &mut device) < 0 || device.is_null() {
        return fail(out, DEVICE_NOT_FOUND);
    }
    // SAFETY: an IGameInputDevice: GetDeviceInfo at 0x18, Release at 0x10.
    let (info, release): (Info, Release) = unsafe {
        let table = *(device as *const *const usize);
        (
            std::mem::transmute(*table.add(3)),
            std::mem::transmute(*table.add(2)),
        )
    };
    // SAFETY: the info holds the device id at +0x20.
    let hr = find_by_id(this, unsafe { info(device).add(0x20) }.cast(), out);
    release(device);
    hr
}
/// Lookup by platform string (console RVA 0x41b0): matched against the
/// device's platform strings, which ours have none of.
extern "system" fn find_device(_: This, _value: *const c_void, out: Out) -> HRESULT {
    fail(out, DEVICE_NOT_FOUND)
}
/// Lookup by platform handle: not implemented by the console's DLL.
extern "system" fn find_by_handle(_: This, _handle: *const c_void, out: Out) -> HRESULT {
    fail(out, E_NOTIMPL)
}
/// E_NOTIMPL on the console as well.
extern "system" fn enable_oem_device(
    _: This,
    _vendor: u16,
    _product: u16,
    _interface: u8,
    _collection: u8,
) -> HRESULT {
    E_NOTIMPL
}
extern "system" fn set_focus_policy(_: This, _policy: u32) {}

static INPUT: Table<22> = Table([
    input_query as *const (),         // 0x00
    input_add_ref as *const (),       // 0x08 AddRef
    input_release as *const (),       // 0x10 Release
    timestamp as *const (),           // 0x18
    current_reading as *const (),     // 0x20
    next_reading as *const (),        // 0x28
    previous_reading as *const (),    // 0x30
    temporal_reading as *const (),    // 0x38
    register_reading as *const (),    // 0x40
    register_device as *const (),     // 0x48
    register_other as *const (),      // 0x50 guide button
    register_other as *const (),      // 0x58 keyboard layout
    stop_callback as *const (),       // 0x60
    unregister_callback as *const (), // 0x68
    create_dispatcher as *const (),   // 0x70
    create_aggregate as *const (),    // 0x78
    find_by_id as *const (),          // 0x80
    find_by_object as *const (),      // 0x88 by object
    find_by_handle as *const (),      // 0x90 by platform handle
    find_device as *const (),         // 0x98 by platform string
    enable_oem_device as *const (),   // 0xa0
    set_focus_policy as *const (),    // 0xa8
]);

extern "system" fn device_info(this: *const Device) -> *const DeviceInfo {
    // SAFETY: one of our devices.
    unsafe { &(*this).info }
}
extern "system" fn device_status(this: *const Device) -> u32 {
    // SAFETY: one of our devices.
    let device = unsafe { &*this };
    if device.hub().state().slots[device.index].connected {
        STATUS_CONNECTED
    } else {
        0
    }
}
// shortcut: trigger motors are dropped (XInput has none). The host API that
// drives them without a redistributable is Windows.Gaming.Input (WinRT
// `Gamepad.Vibration`), which needs WinRT bindings this crate lacks outside
// diagnostics builds; the title sends trigger levels in slot 0x50.
extern "system" fn set_rumble(this: *const Device, params: *const [f32; 4]) {
    // SAFETY: one of our devices.
    let device = unsafe { &*this };
    let one_pad = device.hub().one_pad;
    if device.index >= PADS && !(one_pad && device.index == FALLBACK) {
        return;
    }
    let [low, high, ..] = if params.is_null() {
        [0.0; 4]
    } else {
        // SAFETY: checked non-null; four motor levels.
        unsafe { *params }
    };
    let speed = |level: f32| (level.clamp(0.0, 1.0) * f32::from(u16::MAX)) as u16;
    let mut source = lock(&device.hub().source);
    if one_pad {
        // The one pad is every host pad.
        for slot in 0..PADS {
            if source.pad(slot).is_some() {
                source.rumble(slot, speed(low), speed(high));
            }
        }
    } else {
        source.rumble(device.index, speed(low), speed(high));
    }
}
// The other device methods answer as the console's GameInput.dll does for
// a device with no battery report, no force feedback or haptic motors and
// no raw reports (what ours are).

/// Battery state: rates and capacities 0, status unknown (-1).
extern "system" fn battery_state(_: *const Device, out: *mut [u32; 5]) {
    if !out.is_null() {
        // SAFETY: checked non-null; the title passes a battery state.
        unsafe { out.write([0, 0, 0, 0, u32::MAX]) };
    }
}
/// Force feedback effect: no motor has the index; the effect is cleared.
extern "system" fn create_effect(
    _: *const Device,
    _motor: u32,
    _params: *const c_void,
    out: Out,
) -> HRESULT {
    fail(out, E_BOUNDS)
}
/// Whether a force feedback motor is on: no such motor.
extern "system" fn motor_powered(_: *const Device, _motor: u32) -> bool {
    false
}
/// Haptic motor state: no motor has the index.
extern "system" fn set_haptic(_: *const Device, _motor: u32, _params: *const c_void) -> HRESULT {
    E_BOUNDS
}
/// Methods without a result that change nothing on our devices: motor
/// gain, input synchronization state and hint, power off, the end of
/// exclusive raw access.
extern "system" fn ignore(_: *const Device) {}
/// Raw report: kind 2 (feature) is not implemented; input and output
/// reports need a report the device describes, and ours describe none.
extern "system" fn create_raw_report(_: *const Device, _id: u32, kind: u32, out: Out) -> HRESULT {
    fail(out, if kind == 2 { E_NOTIMPL } else { E_INVALIDARG })
}
/// Raw feature reads and output with a response: not implemented.
extern "system" fn raw_not_implemented(
    _: *const Device,
    _report: *const c_void,
    out: Out,
) -> HRESULT {
    fail(out, E_NOTIMPL)
}
extern "system" fn set_raw_feature(_: *const Device, _report: *const c_void) -> HRESULT {
    E_NOTIMPL
}
/// Raw output: a disconnected device fails as such; ours have no raw
/// output channel.
extern "system" fn send_raw_output(this: *const Device, _report: *const c_void) -> HRESULT {
    if device_status(this) & STATUS_CONNECTED == 0 {
        DEVICE_DISCONNECTED
    } else {
        NOT_SUPPORTED
    }
}
/// Raw IO control: not implemented; the bytes-returned count is cleared.
extern "system" fn raw_io_control(
    _: *const Device,
    _code: u32,
    _in_size: usize,
    _in_buffer: *const c_void,
    _out_size: usize,
    _out_buffer: *mut c_void,
    used: *mut usize,
) -> HRESULT {
    if !used.is_null() {
        // SAFETY: checked non-null.
        unsafe { used.write(0) };
    }
    E_NOTIMPL
}
/// Exclusive raw access: never granted.
extern "system" fn acquire_exclusive(_: *const Device, _timeout: u64) -> bool {
    false
}

static DEVICE: Table<22> = Table([
    device_query as *const (),        // 0x00
    add_ref as *const (),             // 0x08 AddRef
    add_ref as *const (),             // 0x10 Release
    device_info as *const (),         // 0x18
    device_status as *const (),       // 0x20
    battery_state as *const (),       // 0x28
    create_effect as *const (),       // 0x30 force feedback effect
    motor_powered as *const (),       // 0x38
    ignore as *const (),              // 0x40 force feedback motor gain
    set_haptic as *const (),          // 0x48
    set_rumble as *const (),          // 0x50
    ignore as *const (),              // 0x58 input synchronization state
    ignore as *const (),              // 0x60 input synchronization hint
    ignore as *const (),              // 0x68 power off
    create_raw_report as *const (),   // 0x70
    raw_not_implemented as *const (), // 0x78 get raw feature
    set_raw_feature as *const (),     // 0x80
    send_raw_output as *const (),     // 0x88
    raw_not_implemented as *const (), // 0x90 raw output with a response
    raw_io_control as *const (),      // 0x98
    acquire_exclusive as *const (),   // 0xa0
    ignore as *const (),              // 0xa8 release exclusive raw access
]);

fn reading<'a>(this: *const Reading) -> &'a Reading {
    // SAFETY: a reading we handed out and the title still holds.
    unsafe { &*this }
}
extern "system" fn reading_add_ref(this: *const Reading) -> u32 {
    reading(this).refs.fetch_add(1, Ordering::Relaxed) + 1
}
extern "system" fn reading_release(this: *const Reading) -> u32 {
    let left = reading(this).refs.fetch_sub(1, Ordering::AcqRel) - 1;
    if left == 0 {
        // SAFETY: the last reference is gone; `Ref::new` boxed it.
        drop(unsafe { Box::from_raw(this.cast_mut()) });
    }
    left
}
extern "system" fn input_kind(this: *const Reading) -> u32 {
    reading(this).kind()
}
/// One kind's sequence number: a mask of several kinds, or a kind the
/// reading lacks, is 0 (console GameInput.dll).
extern "system" fn sequence_number(this: *const Reading, kind: u32) -> u64 {
    let reading = reading(this);
    if kind.is_power_of_two() && reading.kind() & kind != 0 {
        reading.sequence
    } else {
        0
    }
}
extern "system" fn reading_timestamp(this: *const Reading) -> u64 {
    reading(this).timestamp
}
extern "system" fn reading_device(this: *const Reading, out: *mut *const Device) {
    if !out.is_null() {
        // SAFETY: checked non-null; devices need no reference.
        unsafe { out.write(reading(this).device) };
    }
}
extern "system" fn key_count(this: *const Reading) -> u32 {
    match &reading(this).value {
        Value::Keys(keys) => keys.len() as u32,
        Value::Pad(_) => 0,
    }
}
extern "system" fn key_state(this: *const Reading, count: u32, out: *mut KeyState) -> u32 {
    let Value::Keys(keys) = &reading(this).value else {
        return 0;
    };
    if out.is_null() {
        return 0;
    }
    let n = keys.len().min(count as usize);
    // SAFETY: the title passes room for `count` keys.
    unsafe { std::ptr::copy_nonoverlapping(keys.as_ptr(), out, n) };
    n as u32
}
/// A reading without gamepad state clears the title's (console DLL).
extern "system" fn gamepad_state(this: *const Reading, out: *mut PadState) -> bool {
    if out.is_null() {
        return false;
    }
    let (state, found) = match &reading(this).value {
        Value::Pad(state) => (*state, true),
        Value::Keys(_) => (PadState::default(), false),
    };
    // SAFETY: checked non-null.
    unsafe { out.write(state) };
    found
}
/// Counts and arrays of kinds our devices do not have: none.
extern "system" fn nothing(_: *const c_void) -> usize {
    0
}
/// The state of a kind our readings never carry: as the console's DLL, the
/// title's `BYTES`-byte state is cleared and the result is false (for the
/// raw report, the report pointer is cleared).
extern "system" fn absent<const BYTES: usize>(_: *const Reading, out: *mut u8) -> bool {
    if !out.is_null() {
        // SAFETY: checked non-null; the title passes room for the state.
        unsafe { out.write_bytes(0, BYTES) };
    }
    false
}

static READING: Table<25> = Table([
    reading_query as *const (),     // 0x00
    reading_add_ref as *const (),   // 0x08 AddRef
    reading_release as *const (),   // 0x10
    input_kind as *const (),        // 0x18
    sequence_number as *const (),   // 0x20
    reading_timestamp as *const (), // 0x28
    reading_device as *const (),    // 0x30
    absent::<8> as *const (),       // 0x38 raw report
    nothing as *const (),           // 0x40 controller axes, buttons, switches
    nothing as *const (),           // 0x48
    nothing as *const (),           // 0x50
    nothing as *const (),           // 0x58
    nothing as *const (),           // 0x60
    nothing as *const (),           // 0x68
    key_count as *const (),         // 0x70
    key_state as *const (),         // 0x78
    absent::<0x28> as *const (),    // 0x80 mouse
    nothing as *const (),           // 0x88 touch
    nothing as *const (),           // 0x90
    absent::<0x44> as *const (),    // 0x98 motion
    absent::<4> as *const (),       // 0xa0 arcade stick
    absent::<0x18> as *const (),    // 0xa8 flight stick
    gamepad_state as *const (),     // 0xb0
    absent::<0x1c> as *const (),    // 0xb8 racing wheel
    absent::<4> as *const (),       // 0xc0 UI navigation
]);

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "gameinput",
        dlls: &["gameinput.dll"],
        // Titles import it by ordinal only.
        exports: vec![HleExport {
            name: "#1",
            address: GameInputCreate as *const () as usize,
        }],
    }
}

/// Host input: XInput pads, the keyboard while our process has focus.
#[cfg(windows)]
mod host {
    use std::time::{Duration, Instant};

    use windows_sys::Win32::System::Threading::GetCurrentProcessId;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, MAPVK_VK_TO_VSC_EX, MapVirtualKeyW,
    };
    use windows_sys::Win32::UI::Input::XboxController::{
        XINPUT_GAMEPAD, XINPUT_GAMEPAD_A, XINPUT_GAMEPAD_B, XINPUT_GAMEPAD_BACK,
        XINPUT_GAMEPAD_DPAD_DOWN, XINPUT_GAMEPAD_DPAD_LEFT, XINPUT_GAMEPAD_DPAD_RIGHT,
        XINPUT_GAMEPAD_DPAD_UP, XINPUT_GAMEPAD_LEFT_SHOULDER, XINPUT_GAMEPAD_LEFT_THUMB,
        XINPUT_GAMEPAD_RIGHT_SHOULDER, XINPUT_GAMEPAD_RIGHT_THUMB, XINPUT_GAMEPAD_START,
        XINPUT_GAMEPAD_X, XINPUT_GAMEPAD_Y, XINPUT_STATE, XINPUT_VIBRATION, XInputGetState,
        XInputSetState,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowThreadProcessId,
    };

    use super::*;

    /// Asking XInput about an empty slot is slow; look again this often.
    const EMPTY_SLOT_RECHECK: Duration = Duration::from_secs(1);

    const BUTTONS: [(u16, u32); 14] = [
        (XINPUT_GAMEPAD_START, MENU),
        (XINPUT_GAMEPAD_BACK, VIEW),
        (XINPUT_GAMEPAD_A, A),
        (XINPUT_GAMEPAD_B, B),
        (XINPUT_GAMEPAD_X, X),
        (XINPUT_GAMEPAD_Y, Y),
        (XINPUT_GAMEPAD_DPAD_UP, UP),
        (XINPUT_GAMEPAD_DPAD_DOWN, DOWN),
        (XINPUT_GAMEPAD_DPAD_LEFT, LEFT),
        (XINPUT_GAMEPAD_DPAD_RIGHT, RIGHT),
        (XINPUT_GAMEPAD_LEFT_SHOULDER, LEFT_SHOULDER),
        (XINPUT_GAMEPAD_RIGHT_SHOULDER, RIGHT_SHOULDER),
        (XINPUT_GAMEPAD_LEFT_THUMB, LEFT_STICK),
        (XINPUT_GAMEPAD_RIGHT_THUMB, RIGHT_STICK),
    ];

    /// Raw values, no dead zone: titles apply their own.
    pub(super) fn convert(pad: &XINPUT_GAMEPAD) -> PadState {
        let stick = |v: i16| (f32::from(v) / 32767.0).max(-1.0);
        PadState {
            buttons: BUTTONS
                .iter()
                .filter(|(host, _)| pad.wButtons & host != 0)
                .fold(0, |all, (_, bit)| all | bit),
            left_trigger: f32::from(pad.bLeftTrigger) / 255.0,
            right_trigger: f32::from(pad.bRightTrigger) / 255.0,
            left_x: stick(pad.sThumbLX),
            left_y: stick(pad.sThumbLY),
            right_x: stick(pad.sThumbRX),
            right_y: stick(pad.sThumbRY),
        }
    }

    #[derive(Default)]
    pub(super) struct Host {
        empty_until: [Option<Instant>; PADS],
    }

    impl Source for Host {
        fn pad(&mut self, slot: usize) -> Option<PadState> {
            if self.empty_until[slot].is_some_and(|until| Instant::now() < until) {
                return None;
            }
            let mut state = XINPUT_STATE::default();
            // SAFETY: room for the state.
            if unsafe { XInputGetState(slot as u32, &mut state) } != 0 {
                self.empty_until[slot] = Some(Instant::now() + EMPTY_SLOT_RECHECK);
                return None;
            }
            self.empty_until[slot] = None;
            let mut pad = convert(&state.Gamepad);
            // The scripted keys (`VICTORY_KEYS`) also press the first
            // pad's buttons, by the stand-in pad's table: with a real pad
            // connected the stand-in is not there, and a title that listens
            // to the pad (the title's start screen) saw nothing.
            if slot == 0 {
                for vk in scripted() {
                    if let Some(&(_, button)) =
                        super::FALLBACK_BUTTONS.iter().find(|(key, _)| *key == vk)
                    {
                        pad.buttons |= button;
                    }
                }
            }
            Some(pad)
        }

        fn keys(&mut self) -> Vec<KeyState> {
            let mut down = scripted();
            if focused() {
                // Left/right modifier keys only, as devices report them; no
                // mouse buttons (below 0x08).
                let held: Vec<u8> = (0x08..=0xFE_u8)
                    .filter(|vk| !(0x10..=0x12).contains(vk) && !down.contains(vk))
                    // SAFETY: plain queries.
                    .filter(|&vk| unsafe { GetAsyncKeyState(i32::from(vk)) } < 0)
                    .collect();
                down.extend(held);
            }
            down.into_iter()
                .map(|vk| KeyState {
                    // SAFETY: plain query.
                    scan_code: unsafe { MapVirtualKeyW(u32::from(vk), MAPVK_VK_TO_VSC_EX) },
                    virtual_key: vk,
                    // shortcut: no characters (code point 0);
                    // the title reads scan codes and virtual keys only.
                    ..Default::default()
                })
                .collect()
        }

        fn rumble(&mut self, slot: usize, low: u16, high: u16) {
            let speeds = XINPUT_VIBRATION {
                wLeftMotorSpeed: low,
                wRightMotorSpeed: high,
            };
            // SAFETY: a valid vibration; an empty slot just fails.
            unsafe { XInputSetState(slot as u32, &speeds) };
        }
    }

    /// Debugging aid: `VICTORY_KEYS=<seconds>:<key>,...` holds each key for
    /// 0.2 s that long after input starts (`<from>-<to>:<key>`: all that
    /// time), with or without focus. A key is a
    /// letter, a digit, or `Enter`, `Esc`, `Tab`, `Back`, `Up`, `Down`,
    /// `Left`, `Right` (the stand-in pad's table maps them to buttons:
    /// `Enter` is A). In every build, not only diagnostics ones: a launcher
    /// uses it to press A on the start screen.
    fn scripted() -> Vec<u8> {
        static SCRIPT: LazyLock<(Instant, Vec<(Duration, Duration, u8)>)> = LazyLock::new(|| {
            let list = std::env::var("VICTORY_KEYS").unwrap_or_default();
            let keys = list.split(',').filter_map(|entry| {
                let (at, key) = entry.split_once(':')?;
                let vk = match key.trim() {
                    "Enter" => 0x0D,
                    "Esc" => 0x1B,
                    "Tab" => 0x09,
                    "Back" => 0x08,
                    "Up" => 0x26,
                    "Down" => 0x28,
                    "Left" => 0x25,
                    "Right" => 0x27,
                    k if k.len() == 1 && k.as_bytes()[0].is_ascii_alphanumeric() => {
                        k.as_bytes()[0].to_ascii_uppercase()
                    }
                    _ => return None,
                };
                let time = |t: &str| Duration::try_from_secs_f64(t.trim().parse().ok()?).ok();
                let (from, to) = match at.split_once('-') {
                    Some((from, to)) => (time(from)?, time(to)?),
                    None => (time(at)?, time(at)? + Duration::from_millis(200)),
                };
                Some((from, to, vk))
            });
            (Instant::now(), keys.collect())
        });
        let (start, keys) = &*SCRIPT;
        let now = start.elapsed();
        keys.iter()
            .filter(|&&(from, to, _)| (from..to).contains(&now))
            .map(|&(_, _, vk)| vk)
            .collect()
    }

    /// The foreground window is one of ours (the title's or our fallback).
    fn focused() -> bool {
        let mut process = 0;
        // SAFETY: plain queries; a null window gives process 0.
        unsafe {
            GetWindowThreadProcessId(GetForegroundWindow(), &mut process);
            process == GetCurrentProcessId()
        }
    }

    #[test]
    fn host_pads_convert_to_console_pads() {
        let pad = XINPUT_GAMEPAD {
            wButtons: XINPUT_GAMEPAD_A | XINPUT_GAMEPAD_START | XINPUT_GAMEPAD_DPAD_LEFT,
            bLeftTrigger: 255,
            sThumbLX: -32768,
            sThumbLY: 32767,
            ..Default::default()
        };
        let state = convert(&pad);
        assert_eq!(state.buttons, A | MENU | LEFT);
        assert_eq!(
            (
                state.left_trigger,
                state.left_x,
                state.left_y,
                state.right_x
            ),
            (1.0, -1.0, 1.0, 0.0)
        );
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::HleResolver;
    use crate::HostPolicy;
    use victory_loader::Provenance;
    use victory_pe::ImportSymbol;

    #[derive(Default)]
    struct FakeHost {
        pads: [Option<PadState>; PADS],
        keys: Vec<KeyState>,
        rumble: Vec<(usize, u16, u16)>,
    }

    struct Fake(Arc<Mutex<FakeHost>>);

    impl Source for Fake {
        fn pad(&mut self, slot: usize) -> Option<PadState> {
            self.0.lock().unwrap().pads[slot]
        }
        fn keys(&mut self) -> Vec<KeyState> {
            self.0.lock().unwrap().keys.clone()
        }
        fn rumble(&mut self, slot: usize, low: u16, high: u16) {
            self.0.lock().unwrap().rumble.push((slot, low, high));
        }
    }

    fn fake() -> (&'static Hub, Arc<Mutex<FakeHost>>) {
        fake_with(false)
    }

    fn fake_with(one_pad: bool) -> (&'static Hub, Arc<Mutex<FakeHost>>) {
        let host = Arc::new(Mutex::new(FakeHost::default()));
        let hub = Hub::new(Box::new(Fake(Arc::clone(&host))), one_pad);
        hub.poll();
        (hub, host)
    }

    fn key(virtual_key: u8) -> KeyState {
        KeyState {
            scan_code: u32::from(virtual_key) + 0x100,
            virtual_key,
            ..Default::default()
        }
    }

    fn slot<F: Copy, T>(object: *const T, offset: usize) -> F {
        // SAFETY: our objects start with their table, which has the slot.
        let f = unsafe { *(*(object as *const *const *const ())).add(offset / 8) };
        // SAFETY: the caller names the slot's signature.
        unsafe { std::mem::transmute_copy(&f) }
    }

    /// Device callbacks seen: device index, current and previous status.
    type Log = Mutex<Vec<(usize, u32, u32)>>;

    extern "system" fn record(
        _token: u64,
        context: *mut c_void,
        device: *const Device,
        _timestamp: u64,
        current: u32,
        previous: u32,
    ) {
        // SAFETY: the test passes its log and one of the hub's devices.
        let (log, index) = unsafe { (&*(context as *const Log), (*device).index) };
        log.lock().unwrap().push((index, current, previous));
    }

    fn register(hub: &Hub, kind: u32, enumeration: u32, log: &Log) -> u64 {
        type Register = extern "system" fn(
            This,
            *const Device,
            u32,
            u32,
            u32,
            *mut c_void,
            Option<DeviceCallback>,
            *mut u64,
        ) -> HRESULT;
        let mut token = 0;
        let context = log as *const Log as *mut c_void;
        let register = slot::<Register, _>(&hub.object, 0x48);
        let hr = register(
            &hub.object,
            std::ptr::null(),
            kind,
            1,
            enumeration,
            context,
            Some(record),
            &mut token,
        );
        assert_eq!(hr, S_OK);
        assert_ne!(token, 0);
        token
    }

    fn result(hr: HRESULT, out: *const c_void) -> Result<*const Reading, HRESULT> {
        if hr == S_OK {
            Ok(out.cast())
        } else {
            assert!(out.is_null());
            Err(hr)
        }
    }

    fn current(hub: &Hub, kind: u32, device: usize) -> Result<*const Reading, HRESULT> {
        type Current = extern "system" fn(This, u32, *const Device, Out) -> HRESULT;
        let mut out = std::ptr::null();
        let hr = slot::<Current, _>(&hub.object, 0x20)(
            &hub.object,
            kind,
            &hub.devices[device],
            &mut out,
        );
        result(hr, out)
    }

    fn walk(hub: &Hub, offset: usize, from: *const Reading) -> Result<*const Reading, HRESULT> {
        type Walk = extern "system" fn(This, *const Reading, u32, *const Device, Out) -> HRESULT;
        let mut out = std::ptr::null();
        let device = reading(from).device;
        let hr =
            slot::<Walk, _>(&hub.object, offset)(&hub.object, from, GAMEPAD_KIND, device, &mut out);
        result(hr, out)
    }

    fn pad(reading: *const Reading) -> PadState {
        let mut state = PadState::default();
        let get =
            slot::<extern "system" fn(*const Reading, *mut PadState) -> bool, _>(reading, 0xb0);
        assert!(get(reading, &mut state));
        state
    }

    fn stamp(reading: *const Reading) -> (u64, u64) {
        let time = slot::<extern "system" fn(*const Reading) -> u64, _>(reading, 0x28);
        let sequence = slot::<extern "system" fn(*const Reading, u32) -> u64, _>(reading, 0x20);
        (time(reading), sequence(reading, GAMEPAD_KIND))
    }

    fn release(reading: *const Reading) {
        slot::<extern "system" fn(*const Reading) -> u32, _>(reading, 0x10)(reading);
    }

    #[test]
    fn ordinal_one_resolves_to_create() {
        let r = HleResolver::new(HostPolicy::none());
        assert_eq!(
            r.classify("GameInput.dll", &ImportSymbol::Ordinal(1)),
            Some(Provenance::Hle)
        );
        assert_eq!(r.classify("GameInput.dll", &ImportSymbol::Ordinal(2)), None);
    }

    #[test]
    fn pads_connect_report_and_disconnect() {
        let (hub, host) = fake();
        let log = Log::default();
        register(hub, GAMEPAD_KIND, 2, &log);
        // Blocking enumeration ran before registration returned: with no
        // host pad the keyboard's stand-in is the only one.
        assert_eq!(*log.lock().unwrap(), [(FALLBACK, STATUS_CONNECTED, 0)]);

        host.lock().unwrap().pads[1] = Some(PadState {
            buttons: A,
            left_x: 0.5,
            ..Default::default()
        });
        hub.poll();
        assert_eq!(
            log.lock().unwrap()[1..],
            [(1, STATUS_CONNECTED, 0), (FALLBACK, 0, STATUS_CONNECTED)]
        );

        let first = current(hub, GAMEPAD_KIND, 1).unwrap();
        assert_eq!(pad(first).buttons, A);
        assert_eq!(pad(first).left_x, 0.5);
        // No change, no new reading.
        hub.poll();
        let again = current(hub, GAMEPAD_KIND, 1).unwrap();
        assert_eq!(again, first);
        release(again);

        std::thread::sleep(Duration::from_millis(2));
        host.lock().unwrap().pads[1].as_mut().unwrap().buttons = B;
        hub.poll();
        let second = current(hub, GAMEPAD_KIND, 1).unwrap();
        assert_eq!(pad(second).buttons, B);
        let (t1, s1) = stamp(first);
        let (t2, s2) = stamp(second);
        assert!(t2 > t1 && s2 == s1 + 1);
        assert!(t2 <= now());

        // The title's edge check: previous of current, and back again.
        assert_eq!(walk(hub, 0x30, second), Ok(first));
        assert_eq!(walk(hub, 0x28, first), Ok(second));
        assert_eq!(walk(hub, 0x28, second), Err(READING_NOT_FOUND));
        assert_eq!(walk(hub, 0x30, first), Err(READING_NOT_FOUND));
        for r in [first, first, second, second] {
            release(r);
        }

        host.lock().unwrap().pads[1] = None;
        hub.poll();
        assert_eq!(
            log.lock().unwrap()[3..],
            [(1, 0, STATUS_CONNECTED), (FALLBACK, STATUS_CONNECTED, 0)]
        );
        assert_eq!(current(hub, GAMEPAD_KIND, 1), Err(DEVICE_DISCONNECTED));
        // The stand-in is back, at rest.
        let rest = current(hub, GAMEPAD_KIND, FALLBACK).unwrap();
        assert_eq!(pad(rest), PadState::default());
        release(rest);
    }

    #[test]
    fn keys_feed_the_keyboard_and_the_stand_in_pad() {
        let (hub, host) = fake();
        let log = Log::default();
        register(hub, KEYBOARD_KIND, 2, &log);
        assert_eq!(*log.lock().unwrap(), [(KEYBOARD, STATUS_CONNECTED, 0)]);

        let keys = vec![key(b'W'), key(0x0D), key(b'L'), key(b'C')];
        host.lock().unwrap().keys = keys.clone();
        hub.poll();
        let r = current(hub, KEYBOARD_KIND, KEYBOARD).unwrap();
        let count = slot::<extern "system" fn(*const Reading) -> u32, _>(r, 0x70);
        let state =
            slot::<extern "system" fn(*const Reading, u32, *mut KeyState) -> u32, _>(r, 0x78);
        assert_eq!(count(r), 4);
        let mut got = [KeyState::default(); 8];
        assert_eq!(state(r, 8, got.as_mut_ptr()), 4);
        assert_eq!(got[..4], keys[..]);
        release(r);

        let r = current(hub, GAMEPAD_KIND, FALLBACK).unwrap();
        assert_eq!(
            pad(r),
            PadState {
                buttons: A,
                left_y: 1.0,
                right_x: 1.0,
                right_trigger: 1.0,
                ..Default::default()
            }
        );
        release(r);
        // A keyboard reading is no gamepad reading.
        assert_eq!(current(hub, GAMEPAD_KIND, KEYBOARD), Err(READING_NOT_FOUND));
    }

    #[test]
    fn a_host_pad_feeds_the_one_pad_which_never_goes() {
        let (hub, host) = fake_with(true);
        let log = Log::default();
        register(hub, GAMEPAD_KIND, 2, &log);
        assert_eq!(*log.lock().unwrap(), [(FALLBACK, STATUS_CONNECTED, 0)]);
        let pad = |hub: &Hub| match &hub.state().slots[FALLBACK].history.back().unwrap().value {
            Value::Pad(p) => *p,
            Value::Keys(_) => unreachable!(),
        };
        host.lock().unwrap().keys = vec![key(b'D')];
        hub.poll();
        assert_eq!(pad(hub).left_x, 1.0);
        // A pad is switched on, in any slot: its input arrives, beside the
        // keyboard's, and no device comes or goes.
        host.lock().unwrap().pads[2] = Some(PadState {
            buttons: A,
            left_y: -0.5,
            ..Default::default()
        });
        hub.poll();
        let p = pad(hub);
        assert_eq!((p.buttons, p.left_x, p.left_y), (A, 1.0, -0.5));
        let device: *const Device = &hub.devices[FALLBACK];
        set_rumble(device, &[1.0, 0.0, 0.0, 0.0]);
        assert_eq!(host.lock().unwrap().rumble, [(2, u16::MAX, 0)]);
        // And off again.
        host.lock().unwrap().pads[2] = None;
        host.lock().unwrap().keys.clear();
        hub.poll();
        assert_eq!(pad(hub), PadState::default());
        assert_eq!(log.lock().unwrap().len(), 1);
        assert!(!hub.state().slots[2].connected);
    }

    #[test]
    fn async_enumeration_waits_for_the_poll_and_unregistering_stops_callbacks() {
        let (hub, host) = fake();
        let log = Log::default();
        let token = register(hub, GAMEPAD_KIND, ASYNC_ENUMERATION, &log);
        assert!(log.lock().unwrap().is_empty());
        hub.poll();
        assert_eq!(*log.lock().unwrap(), [(FALLBACK, STATUS_CONNECTED, 0)]);

        let unregister = slot::<extern "system" fn(This, u64, u64) -> bool, _>(&hub.object, 0x68);
        assert!(unregister(&hub.object, token, 5000));
        host.lock().unwrap().pads[0] = Some(PadState::default());
        hub.poll();
        assert_eq!(log.lock().unwrap().len(), 1);
    }

    #[test]
    fn unregistering_waits_for_a_running_callback() {
        use std::sync::atomic::AtomicBool;
        static INSIDE: AtomicBool = AtomicBool::new(false);
        static LEAVE: AtomicBool = AtomicBool::new(false);
        static FROM_INSIDE: AtomicU32 = AtomicU32::new(0);
        extern "system" fn slow(
            token: u64,
            context: *mut c_void,
            _: *const Device,
            _: u64,
            _: u32,
            _: u32,
        ) {
            // From inside its own callback, unregistering fails at once.
            let unregister = slot::<extern "system" fn(This, u64, u64) -> bool, _>(context, 0x68);
            if !unregister(context.cast(), token, 0) {
                FROM_INSIDE.fetch_add(1, Ordering::SeqCst);
            }
            INSIDE.store(true, Ordering::SeqCst);
            while !LEAVE.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        let (hub, host) = fake();
        type Register = extern "system" fn(
            This,
            *const Device,
            u32,
            u32,
            u32,
            *mut c_void,
            Option<DeviceCallback>,
            *mut u64,
        ) -> HRESULT;
        let register = slot::<Register, _>(&hub.object, 0x48);
        let mut token = 0;
        let context = (&raw const hub.object).cast_mut().cast();
        assert_eq!(
            register(
                &hub.object,
                std::ptr::null(),
                GAMEPAD_KIND,
                1,
                0,
                context,
                Some(slow),
                &mut token
            ),
            S_OK
        );
        host.lock().unwrap().pads[1] = Some(PadState::default());
        let poller = std::thread::spawn(|| hub.poll());
        while !INSIDE.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(1));
        }
        let unregister = slot::<extern "system" fn(This, u64, u64) -> bool, _>(&hub.object, 0x68);
        let leave = std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(50));
            LEAVE.store(true, Ordering::SeqCst);
        });
        let start = Instant::now();
        assert!(unregister(&hub.object, token, u64::MAX), "waits for it");
        assert!(start.elapsed() >= Duration::from_millis(40));
        assert!(
            !unregister(&hub.object, token, 0),
            "a token no longer registered"
        );
        assert!(!unregister(&hub.object, 0, 0) && !unregister(&hub.object, u64::MAX, 0));
        assert_eq!(FROM_INSIDE.load(Ordering::SeqCst), 1);
        poller.join().unwrap();
        leave.join().unwrap();
    }

    #[test]
    fn devices_are_found_by_id_and_rumble_reaches_the_host() {
        let (hub, host) = fake();
        host.lock().unwrap().pads[2] = Some(PadState::default());
        hub.poll();
        let device = &hub.devices[2] as *const Device;
        let info = slot::<extern "system" fn(*const Device) -> *const u8, _>(device, 0x18);
        // SAFETY: the info has the id at +0x20.
        let id = unsafe { *(info(device).add(0x20) as *const [u8; 32]) };
        assert_ne!(id, [0; 32]);

        type Find = extern "system" fn(This, *const [u8; 32], Out) -> HRESULT;
        let find = slot::<Find, _>(&hub.object, 0x80);
        let mut out = std::ptr::null();
        assert_eq!(find(&hub.object, &id, &mut out), S_OK);
        assert_eq!(out, device.cast());
        assert_eq!(find(&hub.object, &[1; 32], &mut out), DEVICE_NOT_FOUND);
        assert!(out.is_null());

        type Rumble = extern "system" fn(*const Device, *const [f32; 4]);
        let rumble = slot::<Rumble, _>(device, 0x50);
        rumble(device, &[0.5, 1.0, 1.0, 1.0]);
        rumble(&hub.devices[FALLBACK], &[1.0; 4]);
        rumble(device, std::ptr::null());
        assert_eq!(host.lock().unwrap().rumble, [(2, 32767, 65535), (2, 0, 0)]);
    }

    /// What the console's GameInput.dll answers that a naive layer would
    /// not: missing devices, unimplemented calls, cleared outputs.
    #[test]
    fn answers_follow_the_console_library() {
        let (hub, host) = fake();
        host.lock().unwrap().pads[0] = Some(PadState {
            buttons: A,
            ..Default::default()
        });
        hub.poll();
        let first = current(hub, GAMEPAD_KIND, 0).unwrap();

        // A walk needs its device; the temporal lookup is not implemented.
        type Walk = extern "system" fn(This, *const Reading, u32, *const Device, Out) -> HRESULT;
        let mut out = std::ptr::dangling();
        let next = slot::<Walk, _>(&hub.object, 0x28);
        assert_eq!(
            next(&hub.object, first, GAMEPAD_KIND, std::ptr::null(), &mut out),
            E_NOTIMPL
        );
        assert!(out.is_null());
        let other = &hub.devices[1] as *const Device;
        assert_eq!(
            next(&hub.object, first, GAMEPAD_KIND, other, &mut out),
            E_INVALIDARG
        );
        type Temporal = extern "system" fn(This, u64, *const Device, Out) -> HRESULT;
        let temporal = slot::<Temporal, _>(&hub.object, 0x38);
        out = std::ptr::dangling();
        assert_eq!(
            temporal(&hub.object, now(), &hub.devices[0], &mut out),
            E_NOTIMPL
        );
        assert!(out.is_null());

        // Reading callbacks are not implemented: the token is cleared.
        type ReadingCallback = extern "system" fn(
            This,
            *const c_void,
            u32,
            f32,
            *mut c_void,
            *const c_void,
            *mut u64,
        ) -> HRESULT;
        let mut token = 7;
        let register = slot::<ReadingCallback, _>(&hub.object, 0x40);
        let null = std::ptr::null();
        assert_eq!(
            register(
                &hub.object,
                null,
                GAMEPAD_KIND,
                0.0,
                null.cast_mut(),
                null,
                &mut token
            ),
            E_NOTIMPL
        );
        assert_eq!(token, 0);

        // One kind's sequence number only.
        let sequence = slot::<extern "system" fn(*const Reading, u32) -> u64, _>(first, 0x20);
        assert_ne!(sequence(first, GAMEPAD_KIND), 0);
        assert_eq!(sequence(first, GAMEPAD_KIND | KEYBOARD_KIND), 0);

        // A state the reading lacks is cleared.
        let mouse =
            slot::<extern "system" fn(*const Reading, *mut [u8; 0x28]) -> bool, _>(first, 0x80);
        let mut state = [0xff; 0x28];
        assert!(!mouse(first, &mut state));
        assert_eq!(state, [0; 0x28]);

        // Devices: battery unknown, no motors, no raw access.
        let device = &hub.devices[0] as *const Device;
        let battery = slot::<extern "system" fn(*const Device, *mut [u32; 5]), _>(device, 0x28);
        let mut info = [1; 5];
        battery(device, &mut info);
        assert_eq!(info, [0, 0, 0, 0, u32::MAX]);
        let haptic = slot::<extern "system" fn(*const Device, u32, *const c_void) -> HRESULT, _>(
            device, 0x48,
        );
        assert_eq!(haptic(device, 0, null), E_BOUNDS);
        let acquire = slot::<extern "system" fn(*const Device, u64) -> bool, _>(device, 0xa0);
        assert!(!acquire(device, 0));
        release(first);
    }

    /// Every method of IGameInput, IGameInputDevice and IGameInputReading,
    /// slot by slot, against what the console's GameInput.dll does for the
    /// same call (its function's RVA cited per check).
    #[test]
    fn every_interface_method_matches_the_console_dll() {
        type Qi = extern "system" fn(*const c_void, *const Guid, Out) -> HRESULT;
        type Count = extern "system" fn(*const c_void) -> u32;
        type Found = extern "system" fn(This, *const c_void, Out) -> HRESULT;
        let (hub, host) = fake();
        host.lock().unwrap().pads[1] = Some(PadState {
            buttons: A,
            ..Default::default()
        });
        hub.poll();
        let input: *const c_void = (&raw const hub.object).cast();
        let null = std::ptr::null::<c_void>();
        let other = Guid::new(1, 2, 3, [4; 8]);

        // IGameInput 0x00-0x10: QI 0x7b60, AddRef 0x25c0, Release 0x8800.
        let qi = slot::<Qi, _>(input, 0x00);
        let mut out = std::ptr::dangling();
        for iid in INPUT_IIDS
            .iter()
            .chain(&[IUNKNOWN, IAGILE_OBJECT, INO_MARSHAL])
        {
            assert_eq!(qi(input, iid, &mut out), S_OK);
            assert_eq!(out, input);
        }
        assert_eq!(qi(input, &INPUT_IIDS[0], std::ptr::null_mut()), E_POINTER);
        assert_eq!(qi(input, &other, &mut out), E_NOINTERFACE);
        assert!(out.is_null());
        assert_eq!(qi(input, &DEVICE_IIDS[0], &mut out), E_NOINTERFACE);
        let (add, rel) = (slot::<Count, _>(input, 0x08), slot::<Count, _>(input, 0x10));
        let base = hub.object.refs.load(Ordering::Acquire);
        assert_eq!(base, 5, "five QueryInterface successes, one reference each");
        assert_eq!(add(input), 6);
        assert_eq!(rel(input), 5);

        // 0x18 GetCurrentTimestamp (0x4e70): the microsecond counter.
        let ts = slot::<extern "system" fn(This) -> u64, _>(input, 0x18);
        let (a, b) = (ts(&hub.object), ts(&hub.object));
        assert!(a <= b && b <= now());

        // 0x20 GetCurrentReading (0x4bd0): any device, a device, errors.
        assert!(current(hub, GAMEPAD_KIND, 1).is_ok_and(|r| {
            release(r);
            true
        }));
        assert_eq!(current(hub, GAMEPAD_KIND, 0), Err(DEVICE_DISCONNECTED));
        assert_eq!(
            current(hub, 0x8, 1),
            Err(READING_NOT_FOUND),
            "a kind it lacks"
        );
        type Current = extern "system" fn(This, u32, *const c_void, Out) -> HRESULT;
        let cur = slot::<Current, _>(input, 0x20);
        let stranger = std::ptr::without_provenance::<c_void>(0x10);
        assert_eq!(
            cur(&hub.object, GAMEPAD_KIND, stranger, &mut out),
            E_INVALIDARG
        );
        assert!(out.is_null());
        assert_eq!(cur(&hub.object, GAMEPAD_KIND, null, &mut out), S_OK);
        let any = out.cast::<Reading>();
        assert_eq!(reading(any).device, &raw const hub.devices[1]);

        // 0x48 RegisterDeviceCallback (0x8230), 0x50/0x58 guide button and
        // keyboard layout (0x8630, 0x8470): token cleared, null callback.
        type Other = extern "system" fn(
            This,
            *const c_void,
            *mut c_void,
            *const c_void,
            *mut u64,
        ) -> HRESULT;
        for offset in [0x50, 0x58] {
            let register = slot::<Other, _>(input, offset);
            let mut token = 9;
            assert_eq!(
                register(&hub.object, null, null.cast_mut(), null, &mut token),
                E_INVALIDARG
            );
            assert_eq!(token, 0);
            let callback = std::ptr::without_provenance::<c_void>(1);
            assert_eq!(
                register(&hub.object, null, null.cast_mut(), callback, &mut token),
                S_OK
            );
            assert_ne!(token, 0);
        }
        type RegisterDevice = extern "system" fn(
            This,
            *const c_void,
            u32,
            u32,
            u32,
            *mut c_void,
            *const c_void,
            *mut u64,
        ) -> HRESULT;
        let mut token = 9;
        let register = slot::<RegisterDevice, _>(input, 0x48);
        assert_eq!(
            register(
                &hub.object,
                null,
                GAMEPAD_KIND,
                1,
                0,
                null.cast_mut(),
                null,
                &mut token
            ),
            E_INVALIDARG
        );
        assert_eq!(token, 0);

        // 0x60 StopCallback (0x9b50): 0 and unknown tokens are no-ops.
        let stop = slot::<extern "system" fn(This, u64), _>(input, 0x60);
        stop(&hub.object, 0);
        stop(&hub.object, 12345);
        stop(&hub.object, u64::MAX);

        // 0x70 CreateDispatcher (0x37a0): see the difference list in
        // `create_dispatcher`. 0x78 CreateAggregateDevice (0x3780).
        let aggregate = slot::<extern "system" fn(This, u32, Out) -> HRESULT, _>(input, 0x78);
        out = std::ptr::dangling();
        assert_eq!(aggregate(&hub.object, GAMEPAD_KIND, &mut out), E_NOTIMPL);
        assert!(out.is_null());

        // 0x88 FindDeviceFromObject (0x3f00): a device object is found by
        // its id; an object that is no device is not found.
        let by_object = slot::<Found, _>(input, 0x88);
        let device: *const c_void = (&raw const hub.devices[1]).cast();
        assert_eq!(by_object(&hub.object, device, &mut out), S_OK);
        assert_eq!(out, device);
        assert_eq!(
            by_object(&hub.object, any.cast(), &mut out),
            DEVICE_NOT_FOUND
        );
        assert!(out.is_null());
        // 0x90 by platform handle (0x3780), 0x98 by platform string (0x41b0).
        let by_handle = slot::<Found, _>(input, 0x90);
        out = std::ptr::dangling();
        assert_eq!(
            by_handle(&hub.object, std::ptr::without_provenance(4), &mut out),
            E_NOTIMPL
        );
        assert!(out.is_null());
        let by_string = slot::<Found, _>(input, 0x98);
        let name: [u16; 2] = [b'x'.into(), 0];
        assert_eq!(
            by_string(&hub.object, name.as_ptr().cast(), &mut out),
            DEVICE_NOT_FOUND
        );
        // 0xa0 EnableOemDeviceSupport (0x3bd0), 0xa8 SetFocusPolicy (0x92f0).
        let oem = slot::<extern "system" fn(This, u16, u16, u8, u8) -> HRESULT, _>(input, 0xa0);
        assert_eq!(oem(&hub.object, 1, 2, 3, 4), E_NOTIMPL);
        slot::<extern "system" fn(This, u32), _>(input, 0xa8)(&hub.object, 1);

        // IGameInputDevice: QI 0x7c40, info 0x4fe0, status 0x5000.
        let qi = slot::<Qi, _>(device, 0x00);
        for iid in DEVICE_IIDS
            .iter()
            .chain(&[IUNKNOWN, IAGILE_OBJECT, INO_MARSHAL])
        {
            assert_eq!(qi(device, iid, &mut out), S_OK);
            assert_eq!(out, device);
        }
        assert_eq!(qi(device, &INPUT_IIDS[0], &mut out), E_NOINTERFACE);
        assert!(out.is_null());
        let status = slot::<extern "system" fn(*const c_void) -> u32, _>(device, 0x20);
        assert_eq!(status(device), STATUS_CONNECTED);
        assert_eq!(status((&raw const hub.devices[0]).cast()), 0);
        // 0x30 CreateForceFeedbackEffect (0x38f0), 0x38 motor powered
        // (0x66a0), 0x40 gain (0x9310), 0x58/0x60 synchronization (0x94f0,
        // 0x8e80), 0x68 power off (0x78e0), 0x70 raw report (0x3960), 0x78
        // feature (0x3780), 0x80 set feature (0x3bd0), 0x88 output
        // (0x8f50), 0x90 output with response (0x3780), 0x98 IO control
        // (0x3e30), 0xa8 release exclusive access (0x78e0).
        let effect = slot::<extern "system" fn(*const c_void, u32, *const c_void, Out) -> HRESULT, _>(
            device, 0x30,
        );
        out = std::ptr::dangling();
        assert_eq!(effect(device, 0, null, &mut out), E_BOUNDS);
        assert!(out.is_null());
        assert!(!slot::<extern "system" fn(*const c_void, u32) -> bool, _>(
            device, 0x38
        )(device, 0));
        slot::<extern "system" fn(*const c_void, u32, f32), _>(device, 0x40)(device, 0, 1.0);
        slot::<extern "system" fn(*const c_void, bool), _>(device, 0x58)(device, true);
        slot::<extern "system" fn(*const c_void), _>(device, 0x60)(device);
        slot::<extern "system" fn(*const c_void), _>(device, 0x68)(device);
        let raw =
            slot::<extern "system" fn(*const c_void, u32, u32, Out) -> HRESULT, _>(device, 0x70);
        for (kind, hr) in [(0, E_INVALIDARG), (1, E_INVALIDARG), (2, E_NOTIMPL)] {
            out = std::ptr::dangling();
            assert_eq!(raw(device, 1, kind, &mut out), hr, "report kind {kind}");
            assert!(out.is_null());
        }
        let feature = slot::<extern "system" fn(*const c_void, *const c_void, Out) -> HRESULT, _>(
            device, 0x78,
        );
        assert_eq!(feature(device, null, &mut out), E_NOTIMPL);
        let set_feature =
            slot::<extern "system" fn(*const c_void, *const c_void) -> HRESULT, _>(device, 0x80);
        assert_eq!(set_feature(device, null), E_NOTIMPL);
        let output =
            slot::<extern "system" fn(*const c_void, *const c_void) -> HRESULT, _>(device, 0x88);
        assert_eq!(output(device, null), NOT_SUPPORTED);
        assert_eq!(
            output((&raw const hub.devices[0]).cast(), null),
            DEVICE_DISCONNECTED
        );
        let response = slot::<extern "system" fn(*const c_void, *const c_void, Out) -> HRESULT, _>(
            device, 0x90,
        );
        out = std::ptr::dangling();
        assert_eq!(response(device, null, &mut out), E_NOTIMPL);
        assert!(out.is_null());
        type Ioctl = extern "system" fn(
            *const c_void,
            u32,
            usize,
            *const c_void,
            usize,
            *mut c_void,
            *mut usize,
        ) -> HRESULT;
        let mut used = 7;
        assert_eq!(
            slot::<Ioctl, _>(device, 0x98)(device, 1, 0, null, 0, null.cast_mut(), &mut used),
            E_NOTIMPL
        );
        assert_eq!(used, 0);
        slot::<extern "system" fn(*const c_void), _>(device, 0xa8)(device);

        // IGameInputReading: QI 0x7fc0/0x80f0 (raw report interfaces only
        // with a raw report), kind 0x5100, sequence 0x5cf0, time 0x5ed0,
        // device 0x4f60, raw report 0x5c50, axes/buttons/switches
        // 0x4a60-0x4b30, keys 0x5120/0x5150, mouse 0x5270, touch
        // 0x5ee0/0x5f10, motion 0x51f0, arcade 0x4920, flight 0x5010,
        // gamepad 0x50b0, wheel 0x5b20, UI navigation 0x5f60.
        let r: *const c_void = any.cast();
        let qi = slot::<Qi, _>(r, 0x00);
        let refs = |x: *const c_void| reading(x.cast()).refs.load(Ordering::Acquire);
        let before = refs(r);
        for iid in READING_IIDS
            .iter()
            .chain(&[IUNKNOWN, IAGILE_OBJECT, INO_MARSHAL])
        {
            assert_eq!(qi(r, iid, &mut out), S_OK);
            assert_eq!(out, r);
        }
        assert_eq!(refs(r), before + 5);
        for _ in 0..5 {
            release(any);
        }
        let raw_report = Guid::new(
            0x61f08cf1,
            0x1ffc,
            0x40ca,
            [0xa2, 0xb8, 0xe1, 0xab, 0x8b, 0xc5, 0xb6, 0xdc],
        );
        assert_eq!(qi(r, &raw_report, &mut out), E_NOINTERFACE);
        assert!(out.is_null());
        assert_eq!(
            slot::<extern "system" fn(*const c_void) -> u32, _>(r, 0x18)(r),
            GAMEPAD_KIND
        );
        let mut dev = std::ptr::null();
        slot::<extern "system" fn(*const c_void, *mut *const c_void), _>(r, 0x30)(r, &mut dev);
        assert_eq!(dev, device);
        let mut report = std::ptr::dangling::<c_void>();
        assert!(!slot::<
            extern "system" fn(*const c_void, *mut *const c_void) -> bool,
            _,
        >(r, 0x38)(r, &mut report));
        assert!(report.is_null());
        for offset in [0x40, 0x50, 0x60, 0x70, 0x88] {
            assert_eq!(
                slot::<extern "system" fn(*const c_void) -> u32, _>(r, offset)(r),
                0,
                "count {offset:#x}"
            );
        }
        let mut sink = [0xffu8; 0x80];
        for offset in [0x48, 0x58, 0x68, 0x78, 0x90] {
            let array =
                slot::<extern "system" fn(*const c_void, u32, *mut u8) -> u32, _>(r, offset);
            assert_eq!(array(r, 4, sink.as_mut_ptr()), 0, "array {offset:#x}");
        }
        assert_eq!(sink, [0xff; 0x80], "arrays write nothing");
        for (offset, bytes) in [
            (0x80, 0x28),
            (0x98, 0x44),
            (0xa0, 4),
            (0xa8, 0x18),
            (0xb8, 0x1c),
            (0xc0, 4),
        ] {
            let mut state = [0xffu8; 0x80];
            let get = slot::<extern "system" fn(*const c_void, *mut u8) -> bool, _>(r, offset);
            assert!(!get(r, state.as_mut_ptr()), "state {offset:#x}");
            assert!(
                state[..bytes].iter().all(|&b| b == 0) && state[bytes..].iter().all(|&b| b == 0xff),
                "state {offset:#x} clears {bytes:#x} bytes"
            );
        }
        let mut pad = PadState {
            left_x: 0.5,
            ..Default::default()
        };
        assert!(slot::<
            extern "system" fn(*const c_void, *mut PadState) -> bool,
            _,
        >(r, 0xb0)(r, &mut pad));
        assert_eq!(pad.buttons, A);
        release(any);
    }

    /// Nanoseconds per call of `f`, the best of five runs of `n` calls.
    pub(crate) fn time(label: &str, n: u32, mut f: impl FnMut()) {
        let best = (0..5)
            .map(|_| {
                let start = Instant::now();
                for _ in 0..n {
                    f();
                }
                start.elapsed().as_nanos() as f64 / f64::from(n)
            })
            .fold(f64::INFINITY, f64::min);
        println!("bench {label}: {best:.1} ns");
    }

    /// The per-frame reads and the poll. Run with `--ignored --nocapture
    /// bench_` on a release test build.
    #[test]
    #[ignore]
    fn bench_input_hot_paths() {
        let (hub, host) = fake_with(true);
        type Current = extern "system" fn(This, u32, *const Device, Out) -> HRESULT;
        let cur = slot::<Current, _>(&hub.object, 0x20);
        let mut out = std::ptr::null();
        time("GetCurrentReading(any device)+Release", 200_000, || {
            cur(&hub.object, GAMEPAD_KIND, std::ptr::null(), &mut out);
            release(std::hint::black_box(out).cast());
        });
        time("GetCurrentReading(device)+Release", 200_000, || {
            cur(&hub.object, GAMEPAD_KIND, &hub.devices[FALLBACK], &mut out);
            release(std::hint::black_box(out).cast());
        });
        cur(&hub.object, GAMEPAD_KIND, &hub.devices[FALLBACK], &mut out);
        let r = out.cast::<Reading>();
        let get = slot::<extern "system" fn(*const Reading, *mut PadState) -> bool, _>(r, 0xb0);
        let mut pad = PadState::default();
        time("GetGamepadState", 1_000_000, || {
            std::hint::black_box(get(r, &mut pad));
        });
        release(r);
        time("poll, nothing changed", 20_000, || hub.poll());
        let mut flip = 0u32;
        time("poll, the pad changed", 20_000, || {
            flip ^= A;
            host.lock().unwrap().pads[0] = Some(PadState {
                buttons: flip,
                ..Default::default()
            });
            hub.poll();
        });
    }
}
