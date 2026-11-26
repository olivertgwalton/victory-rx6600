//! Thread-locals that cost what a thread-local should: [`fast_local!`].
//!
//! The layer is built for `x86_64-pc-windows-gnu`, where std has no native
//! thread-locals (`target_thread_local` is off) and keeps every
//! `thread_local!` under an OS key with a destructor. Every access then
//! calls `std::sys::thread_local::guard::windows::enable` (`LazyKey::force`
//! does, for any key with a destructor), and on this target that call
//! cannot remember having run on the thread: each time it adds to and takes
//! from one counter shared by the whole process, issues a `SeqCst` fence and
//! sets a fiber-local value through Wine. The recording threads, which touch
//! thread-locals for every draw and descriptor, spent 34% of their own code's
//! time there (profile base: 21% the shared counter's line, 14% the fence,
//! 7% the rest of `enable`), all eight of them on one cache line.
//!
//! [`fast_local!`] declares the same thing on Windows' fiber-local storage
//! directly: one `FlsAlloc` per variable (with the destructor that drops a
//! thread's value when the thread ends), and per access one `FlsGetValue`,
//! which reads the thread's own table: no shared counter, no fence. A
//! thread's value is boxed at its first access, by the system's allocator,
//! never the global one (as std's are): a global allocator may itself use
//! thread-locals (the diagnostics build's counting one does), and boxing
//! through it would ask it again before the value is there, without end.
//! Elsewhere (the native unit
//! tests) it is std's `thread_local!`. The methods are `LocalKey`'s: `with`,
//! and for cells `get`, `set`, `take`, `replace`, `with_borrow`,
//! `with_borrow_mut`.

/// Declares thread-locals as `thread_local!` does (the same syntax, `const`
/// initializers included), stored as [`FastLocal`]s on Windows. See the
/// module.
#[macro_export]
macro_rules! fast_local {
    () => {};
    ($(#[$attr:meta])* $vis:vis static $name:ident : $t:ty = const { $init:expr } $(; $($rest:tt)*)?) => {
        $crate::fast_local!(@one [$(#[$attr])*] [$vis] $name, $t, $init);
        $($crate::fast_local!($($rest)*);)?
    };
    ($(#[$attr:meta])* $vis:vis static $name:ident : $t:ty = $init:expr $(; $($rest:tt)*)?) => {
        $crate::fast_local!(@one [$(#[$attr])*] [$vis] $name, $t, $init);
        $($crate::fast_local!($($rest)*);)?
    };
    (@one [$($attr:tt)*] [$($vis:tt)*] $name:ident, $t:ty, $init:expr) => {
        #[cfg(windows)]
        $($attr)* $($vis)* static $name: $crate::fast_local::FastLocal<$t> = $crate::fast_local::FastLocal::new({
            fn __init() -> $t {
                $init
            }
            __init
        });
        #[cfg(not(windows))]
        ::std::thread_local! {
            $($attr)* $($vis)* static $name: $t = $init;
        }
    };
}

#[cfg(windows)]
pub use imp::FastLocal;

#[cfg(windows)]
mod imp {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::{Cell, RefCell};
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicU32, Ordering};

    use windows_sys::Win32::System::Threading::{FLS_OUT_OF_INDEXES, FlsAlloc, FlsFree, FlsGetValue, FlsSetValue};

    /// A thread-local value of `T`: see the module. Made by [`fast_local!`].
    pub struct FastLocal<T: 'static> {
        /// The fiber-local slot, plus one (0: none yet).
        index: AtomicU32,
        init: fn() -> T,
    }

    // SAFETY: what it hands out is the calling thread's own value; the slot
    // number and the initializer are shared, and are plain data.
    unsafe impl<T: 'static> Sync for FastLocal<T> {}

    /// Drops a thread's value when the thread (or fiber) ends.
    unsafe extern "system" fn drop_value<T>(value: *const c_void) {
        if !value.is_null() {
            // SAFETY: the value `FastLocal::make` put in the slot, given back
            // once by the system as the slot's owner goes.
            unsafe { free::<T>(value as *mut T) };
        }
    }

    /// `value` in memory of the system's allocator.
    fn boxed<T>(value: T) -> *mut T {
        let layout = Layout::new::<T>();
        if layout.size() == 0 {
            std::mem::forget(value);
            return std::ptr::NonNull::<T>::dangling().as_ptr();
        }
        // SAFETY: a layout of non-zero size.
        let at = unsafe { System.alloc(layout) } as *mut T;
        if at.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        // SAFETY: fresh memory of `T`'s layout.
        unsafe { at.write(value) };
        at
    }

    /// Drops and frees what [`boxed`] made.
    ///
    /// # Safety
    /// `at` came from [`boxed`] and is not used again.
    unsafe fn free<T>(at: *mut T) {
        let layout = Layout::new::<T>();
        // SAFETY: the caller's.
        unsafe {
            std::ptr::drop_in_place(at);
            if layout.size() != 0 {
                System.dealloc(at.cast(), layout);
            }
        }
    }

    impl<T: 'static> FastLocal<T> {
        #[doc(hidden)]
        pub const fn new(init: fn() -> T) -> Self {
            FastLocal { index: AtomicU32::new(0), init }
        }

        #[inline]
        fn index(&self) -> u32 {
            match self.index.load(Ordering::Acquire) {
                0 => self.allocate(),
                n => n - 1,
            }
        }

        #[cold]
        fn allocate(&self) -> u32 {
            // SAFETY: a slot whose destructor drops what `make` boxes.
            let index = unsafe { FlsAlloc(Some(drop_value::<T>)) };
            if index == FLS_OUT_OF_INDEXES {
                std::process::abort();
            }
            match self.index.compare_exchange(0, index + 1, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => index,
                Err(theirs) => {
                    // Another thread's slot came first; no value is in ours.
                    // SAFETY: ours, unused.
                    unsafe { FlsFree(index) };
                    theirs - 1
                }
            }
        }

        /// The value made for this thread at its first access.
        #[cold]
        fn make(&self, index: u32) -> *mut T {
            let value = boxed((self.init)());
            // SAFETY: this thread's slot. A value the initializer made for
            // itself meanwhile goes (as std's does): nothing borrows it yet.
            unsafe {
                let made = FlsGetValue(index) as *mut T;
                FlsSetValue(index, value as *const c_void);
                if !made.is_null() {
                    free(made);
                }
            }
            value
        }

        /// Calls `f` with this thread's value.
        #[inline]
        pub fn with<R>(&'static self, f: impl FnOnce(&T) -> R) -> R {
            let index = self.index();
            // SAFETY: this thread's slot, holding null or the value `make`
            // boxed, which lives until the thread ends.
            let mut value = unsafe { FlsGetValue(index) } as *mut T;
            if value.is_null() {
                value = self.make(index);
            }
            // SAFETY: as above; only this thread reaches it.
            f(unsafe { &*value })
        }

        /// As `LocalKey::try_with`: never fails here.
        #[inline]
        pub fn try_with<R>(&'static self, f: impl FnOnce(&T) -> R) -> Result<R, std::thread::AccessError> {
            Ok(self.with(f))
        }
    }

    impl<T: Copy + 'static> FastLocal<Cell<T>> {
        #[inline]
        pub fn get(&'static self) -> T {
            self.with(Cell::get)
        }

        #[inline]
        pub fn set(&'static self, value: T) {
            self.with(|c| c.set(value));
        }

        #[inline]
        pub fn replace(&'static self, value: T) -> T {
            self.with(|c| c.replace(value))
        }
    }

    impl<T: Default + Copy + 'static> FastLocal<Cell<T>> {
        #[inline]
        pub fn take(&'static self) -> T {
            self.with(Cell::take)
        }
    }

    impl<T: 'static> FastLocal<RefCell<T>> {
        #[inline]
        pub fn with_borrow<R>(&'static self, f: impl FnOnce(&T) -> R) -> R {
            self.with(|c| f(&c.borrow()))
        }

        #[inline]
        pub fn with_borrow_mut<R>(&'static self, f: impl FnOnce(&mut T) -> R) -> R {
            self.with(|c| f(&mut c.borrow_mut()))
        }

        #[inline]
        pub fn set(&'static self, value: T) {
            self.with(|c| *c.borrow_mut() = value);
        }

        #[inline]
        pub fn replace(&'static self, value: T) -> T {
            self.with(|c| c.replace(value))
        }
    }

    impl<T: Default + 'static> FastLocal<RefCell<T>> {
        #[inline]
        pub fn take(&'static self) -> T {
            self.with(|c| c.take())
        }
    }
}

#[cfg(test)]
mod tests {
    crate::fast_local! {
        static COUNT: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
        static LIST: std::cell::RefCell<Vec<u32>> = const { std::cell::RefCell::new(Vec::new()) };
    }

    /// Each thread has its own values, as with `thread_local!`.
    #[test]
    fn each_thread_has_its_own() {
        COUNT.with(|c| c.set(c.get() + 1));
        LIST.with_borrow_mut(|l| l.push(7));
        std::thread::spawn(|| {
            assert_eq!(COUNT.with(|c| c.get()), 0);
            LIST.with_borrow(|l| assert!(l.is_empty()));
        })
        .join()
        .unwrap();
        assert_eq!(COUNT.with(|c| c.get()), 1);
        LIST.with_borrow(|l| assert_eq!(l, &[7]));
    }
}
