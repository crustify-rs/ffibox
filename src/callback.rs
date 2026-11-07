//! Rust code C calls: closures behind a C function pointer and its `void *`
//! context, and the panic barrier between them.
//!
//! A C API takes a callback as a function pointer and a context pointer it
//! passes back on every call, first (`fn(void *ctx, …)`) or last
//! (`fn(…, void *ctx)`). Two lifetimes cover the APIs that do:
//!
//! | C keeps the callback | Primitive | Rust counterpart |
//! |----------------------|-----------|------------------|
//! | for one call (`foreach`, `qsort_r`) | [`CClosure`] | `&mut F` passed down |
//! | after the call returns, until it releases it (`set_log_callback`) | [`CPayload`] | `Box<F>` handed over |
//!
//! Each gives the trampoline, an `unsafe extern "C" fn` with the C type's
//! signature that recovers the closure from the context and calls it, and the
//! context itself. The trampoline's type is the one the C declaration names,
//! so a closure of the wrong arity or argument types does not compile:
//!
//! ```
//! use core::ffi::{c_int, c_void};
//! use ffibox::callback::CClosure;
//!
//! // A C routine calling `cb(x, ctx)` for each element.
//! unsafe extern "C" fn foreach(
//!     xs: *const c_int,
//!     n: usize,
//!     cb: Option<unsafe extern "C" fn(c_int, *mut c_void) -> c_int>,
//!     ctx: *mut c_void,
//! ) -> c_int {
//!     for i in 0..n {
//!         let rc = unsafe { cb.unwrap()(*xs.add(i), ctx) };
//!         if rc != 0 {
//!             return rc;
//!         }
//!     }
//!     0
//! }
//!
//! let xs = [1, 2, 3];
//! let mut sum = 0;
//! let mut add = |x: c_int| {
//!     sum += x;
//!     0
//! };
//! let cb = CClosure::new(&mut add, -1);
//! // SAFETY: `foreach` calls the callback only during the call, with `ctx`.
//! let rc = unsafe { foreach(xs.as_ptr(), xs.len(), Some(cb.fn_ctx_last()), cb.ctx()) };
//! cb.finish();
//! assert_eq!((rc, sum), (0, 6));
//! ```
//!
//! The closure's arguments are the C types: the binding converts them to
//! handles and calls its consumer's closure with those, so the consumer's
//! closure takes safe types only.

use core::ffi::c_void;
use core::marker::PhantomData;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, Ordering};

/// Run `f`, and turn a panic into `on_panic()` rather than let it unwind
/// into C: the barrier an exported function or a callback puts around its
/// body, mapping a panic to its error convention, like
/// [`std::panic::catch_unwind`].
///
/// A panic that reaches an `extern "C"` function aborts the process, which
/// is sound but takes the host with it; the barrier keeps it to the call. The
/// state `f` left behind is the caller's to treat as poisoned, as after any
/// caught panic.
///
/// ```
/// use core::ffi::c_int;
///
/// extern "C" fn parse(n: c_int) -> c_int {
///     ffibox::callback::barrier(
///         || {
///             assert!(n >= 0, "negative input");
///             n * 2
///         },
///         || -1,
///     )
/// }
/// # let hook = std::panic::take_hook();
/// # std::panic::set_hook(Box::new(|_| {}));
/// assert_eq!(parse(-3), -1);
/// # std::panic::set_hook(hook);
/// assert_eq!(parse(3), 6);
/// ```
#[cfg(feature = "std")]
#[cfg_attr(docsrs, doc(cfg(feature = "std")))]
#[inline]
pub fn barrier<R>(f: impl FnOnce() -> R, on_panic: impl FnOnce() -> R) -> R {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or_else(|_| on_panic())
}

// ---------------------------------------------------------------------------
// The trampoline shapes
// ---------------------------------------------------------------------------

/// The C function pointer types a callback holder offers with its context
/// passed **first**, `unsafe extern "C" fn(*mut c_void, A, B, …) -> R`.
/// Implemented for every arity up to 6; reached through
/// [`CClosure::fn_ctx_first`] and [`CPayload::fn_ctx_first`].
pub trait CtxFirst<P> {
    /// The trampoline.
    #[doc(hidden)]
    fn trampoline() -> P;
}

/// The C function pointer types a callback holder offers with its context
/// passed **last**, `unsafe extern "C" fn(A, B, …, *mut c_void) -> R`. As
/// [`CtxFirst`].
pub trait CtxLast<P> {
    /// The trampoline.
    #[doc(hidden)]
    fn trampoline() -> P;
}

// ---------------------------------------------------------------------------
// CClosure — a closure C calls during one call
// ---------------------------------------------------------------------------

/// The state a [`CClosure`]'s context points at.
struct ScopedState<F, R> {
    f: NonNull<F>,
    on_failure: R,
    /// Taken by each invocation, so a re-entrant or concurrent one is
    /// refused rather than given a second `&mut F`.
    busy: AtomicBool,
    /// Set when an invocation was refused; reported by `finish`.
    refused: AtomicBool,
    /// The first panic the closure raised, resumed by `finish`. Written only
    /// by the invocation holding `busy`.
    #[cfg(feature = "std")]
    panic: core::cell::UnsafeCell<Option<std::boxed::Box<dyn core::any::Any + Send>>>,
}

/// A closure C calls for the duration of one call, through a trampoline and
/// a context pointer: `&mut F` passed down to C.
///
/// Built from `&mut F`, so the closure may capture and change local state.
/// [`ctx`](Self::ctx) is the context to pass, and
/// [`fn_ctx_first`](Self::fn_ctx_first) / [`fn_ctx_last`](Self::fn_ctx_last)
/// the trampoline, typed as the C declaration names it. After the call,
/// [`finish`](Self::finish) (or dropping the `CClosure`) resumes a panic the
/// closure raised.
///
/// **Failures are values.** A panic in the closure does not unwind into C:
/// with `std`, the trampoline catches it, returns `on_failure`, and refuses
/// every later invocation the same way, so the C routine stops as its
/// convention says; `finish` then resumes the panic. Without `std`, a panic
/// aborts, as an `extern "C"` function's does. An invocation while another is
/// running, re-entrant through a nested C call or from another thread, is
/// refused with `on_failure` too, since a second `&mut F` would alias the
/// first, and `finish` reports it.
///
/// **Threads.** [`new`](Self::new) requires `F: Send`, since a C routine may
/// call back on a thread of its own; [`new_local`](Self::new_local) drops the
/// bound for a routine that calls back on the calling thread only.
///
/// **Lifetime.** The context is valid while the `CClosure` lives and is not
/// moved: a C routine that keeps the callback after it returns needs a
/// [`CPayload`] instead.
#[must_use = "a CClosure does nothing until C calls its trampoline"]
pub struct CClosure<'a, F, R: Copy> {
    state: ScopedState<F, R>,
    _borrow: PhantomData<&'a mut F>,
}

impl<'a, F, R: Copy> CClosure<'a, F, R> {
    /// Wrap `f` for C, returning `on_failure` from any invocation that cannot
    /// run it (after a panic, or while another invocation runs).
    #[inline]
    pub fn new(f: &'a mut F, on_failure: R) -> Self
    where
        F: Send,
    {
        // SAFETY: `F: Send` lets any thread run the closure.
        unsafe { Self::new_local(f, on_failure) }
    }

    /// As [`new`](Self::new), for a closure that need not be `Send`.
    ///
    /// # Safety
    ///
    /// The C routine given the trampoline calls it on the calling thread
    /// only.
    #[inline]
    pub unsafe fn new_local(f: &'a mut F, on_failure: R) -> Self {
        Self {
            state: ScopedState {
                f: NonNull::from(f),
                on_failure,
                busy: AtomicBool::new(false),
                refused: AtomicBool::new(false),
                #[cfg(feature = "std")]
                panic: core::cell::UnsafeCell::new(None),
            },
            _borrow: PhantomData,
        }
    }

    /// The context pointer to pass with the trampoline.
    ///
    /// Valid while `self` lives and is not moved; the borrow a C call takes
    /// of `self` to pass it keeps both true for the call. Not for a C
    /// routine that keeps it after returning.
    #[inline]
    #[must_use]
    pub fn ctx(&self) -> *mut c_void {
        core::ptr::from_ref(&self.state).cast_mut().cast()
    }

    /// The trampoline for a C callback type taking its context first,
    /// `unsafe extern "C" fn(*mut c_void, A, …) -> R`, inferred from where it
    /// is passed.
    #[inline]
    #[must_use]
    pub fn fn_ctx_first<P>(&self) -> P
    where
        Self: CtxFirst<P>,
    {
        <Self as CtxFirst<P>>::trampoline()
    }

    /// The trampoline for a C callback type taking its context last,
    /// `unsafe extern "C" fn(A, …, *mut c_void) -> R`, inferred from where it
    /// is passed.
    #[inline]
    #[must_use]
    pub fn fn_ctx_last<P>(&self) -> P
    where
        Self: CtxLast<P>,
    {
        <Self as CtxLast<P>>::trampoline()
    }

    /// Whether an invocation failed: the closure panicked, or an invocation
    /// was refused while another ran.
    #[inline]
    #[must_use]
    pub fn failed(&self) -> bool {
        self.state.refused.load(Ordering::Acquire) || self.panicked()
    }

    #[cfg(feature = "std")]
    fn panicked(&self) -> bool {
        // SAFETY: called with no invocation running (the C call that could
        // run one has returned, which a scoped callback's contract orders
        // before this), so nothing writes the slot meanwhile.
        unsafe { (*self.state.panic.get()).is_some() }
    }

    #[cfg(not(feature = "std"))]
    fn panicked(&self) -> bool {
        false
    }

    /// End the callback's use: resume the panic the closure raised, if any,
    /// and panic if an invocation was refused. Dropping the `CClosure` does
    /// the same, unless the thread is already panicking.
    #[inline]
    pub fn finish(self) {
        drop(self);
    }
}

impl<F, R: Copy> Drop for CClosure<'_, F, R> {
    fn drop(&mut self) {
        #[cfg(feature = "std")]
        {
            if std::thread::panicking() {
                return;
            }
            // SAFETY: the C call has returned, so no invocation runs.
            if let Some(payload) = unsafe { (*self.state.panic.get()).take() } {
                std::panic::resume_unwind(payload);
            }
        }
        if self.state.refused.load(Ordering::Acquire) {
            panic!("ffibox: a CClosure was invoked while another invocation of it was running");
        }
    }
}

impl<F, R: Copy> core::fmt::Debug for CClosure<'_, F, R> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CClosure")
            .field("failed", &self.failed())
            .finish()
    }
}

/// Run one invocation of a [`CClosure`] through its context.
///
/// # Safety
///
/// `ctx` is the context of a live `CClosure<'_, F, R>`.
unsafe fn invoke_scoped<F, R: Copy>(ctx: *mut c_void, call: impl FnOnce(&mut F) -> R) -> R {
    // SAFETY: the caller passes a live `ScopedState<F, R>`, shared between
    // invocations; its mutable parts are atomics and the panic slot the
    // `busy` holder alone writes.
    let state = unsafe { &*ctx.cast::<ScopedState<F, R>>() };
    if state.busy.swap(true, Ordering::Acquire) {
        state.refused.store(true, Ordering::Release);
        return state.on_failure;
    }
    #[cfg(feature = "std")]
    let ret = {
        // SAFETY: `busy` makes this the only invocation, so the slot is ours.
        let slot = unsafe { &mut *state.panic.get() };
        if slot.is_some() {
            state.on_failure
        } else {
            // SAFETY: `f` came from the `&'a mut F` the `CClosure` borrows,
            // and `busy` makes this the only `&mut F` derived from it.
            let f = unsafe { &mut *state.f.as_ptr() };
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| call(f))) {
                Ok(ret) => ret,
                Err(payload) => {
                    *slot = Some(payload);
                    state.on_failure
                }
            }
        }
    };
    #[cfg(not(feature = "std"))]
    // SAFETY: as above; a panic unwinds to the `extern "C"` trampoline, which
    // aborts.
    let ret = call(unsafe { &mut *state.f.as_ptr() });
    state.busy.store(false, Ordering::Release);
    ret
}

// ---------------------------------------------------------------------------
// CPayload — a Rust value C holds
// ---------------------------------------------------------------------------

/// A Rust value C holds behind a `void *` until it releases it: the user
/// data of a registered callback, the payload of a C container. `Box<T>`
/// handed over to C.
///
/// [`into_raw`](Self::into_raw) surrenders it to C as a context pointer;
/// [`release_fn`](Self::release_fn) is the `void (*)(void *)` C calls once to
/// drop it; [`borrow`](Self::borrow) recovers a shared reference inside a
/// callback. For a closure, [`fn_ctx_first`](Self::fn_ctx_first) /
/// [`fn_ctx_last`](Self::fn_ctx_last) give the trampoline that calls it.
///
/// C reaches the value only through its pointer, never its bytes, so the
/// value is a Rust value throughout: a `&T` to it asserts nothing about
/// memory C writes. Its address is stable from `new` to the release, as a
/// `Box`'s is.
///
/// **Shared access only.** C may call a registered callback re-entrantly or
/// from several threads, so a callback reaches the value as `&T`, and a
/// closure is an `Fn`; state it changes goes behind a `Cell`, `Mutex` or
/// atomic, as state shared through an `Arc` does. [`into_raw`](Self::into_raw)
/// requires `T: Send + Sync` for that reason;
/// [`into_raw_local`](Self::into_raw_local) drops the bound for a C library
/// that calls back and releases on the registering thread only.
///
/// **Panics** in a registered closure abort the process, as in any `extern
/// "C"` function: there is no Rust caller to resume them in. A closure that
/// can fail wraps its body in [`barrier`] and returns its error value.
///
/// ```
/// use core::ffi::{c_int, c_void};
/// use std::sync::atomic::{AtomicI32, Ordering};
/// use ffibox::callback::CPayload;
///
/// // A C library that keeps a callback and its context until reset.
/// struct Lib {
///     cb: Option<unsafe extern "C" fn(*mut c_void, c_int)>,
///     ctx: *mut c_void,
///     free: Option<unsafe extern "C" fn(*mut c_void)>,
/// }
/// impl Lib {
///     fn emit(&self, v: c_int) {
///         if let Some(cb) = self.cb {
///             unsafe { cb(self.ctx, v) }
///         }
///     }
///     fn reset(&mut self) {
///         if let Some(free) = self.free.take() {
///             unsafe { free(self.ctx) }
///         }
///     }
/// }
///
/// let total = std::sync::Arc::new(AtomicI32::new(0));
/// let seen = total.clone();
/// let payload = CPayload::new(move |v: c_int| {
///     seen.fetch_add(v, Ordering::Relaxed);
/// });
/// let mut lib = Lib {
///     cb: Some(payload.fn_ctx_first()),
///     free: Some(CPayload::<_>::release_fn_of(&payload)),
///     ctx: payload.into_raw(),
/// };
/// lib.emit(2);
/// lib.emit(3);
/// lib.reset(); // drops the closure
/// assert_eq!(total.load(Ordering::Relaxed), 5);
/// ```
#[cfg(feature = "alloc")]
#[cfg_attr(docsrs, doc(cfg(feature = "alloc")))]
#[must_use = "dropping a CPayload drops its value"]
pub struct CPayload<T>(alloc::boxed::Box<T>);

#[cfg(feature = "alloc")]
impl<T> CPayload<T> {
    /// Move `value` to the heap, ready to hand to C.
    #[inline]
    pub fn new(value: T) -> Self {
        Self(alloc::boxed::Box::new(value))
    }

    /// Surrender the value to C as a context pointer. C releases it once,
    /// through [`release_fn`](Self::release_fn); until then it may call back
    /// with it, on any thread.
    #[inline]
    #[must_use = "the returned pointer owns the value and must be released"]
    pub fn into_raw(self) -> *mut c_void
    where
        T: Send + Sync,
    {
        // SAFETY: `T: Send + Sync` lets any thread share and release it.
        unsafe { self.into_raw_local() }
    }

    /// As [`into_raw`](Self::into_raw), for a value that need not be `Send`
    /// or `Sync`.
    ///
    /// # Safety
    ///
    /// The C library calls back with the pointer, and releases it, on the
    /// calling thread only.
    #[inline]
    #[must_use = "the returned pointer owns the value and must be released"]
    pub unsafe fn into_raw_local(self) -> *mut c_void {
        alloc::boxed::Box::into_raw(self.0).cast()
    }

    /// Take the value back from C.
    ///
    /// # Safety
    ///
    /// `ptr` came from [`into_raw`](Self::into_raw) or
    /// [`into_raw_local`](Self::into_raw_local) of a `CPayload<T>`, C will
    /// neither use nor release it again, and no reference from
    /// [`borrow`](Self::borrow) is live.
    #[inline]
    pub unsafe fn from_raw(ptr: *mut c_void) -> Self {
        // SAFETY: the caller hands back the box `into_raw` surrendered.
        Self(unsafe { alloc::boxed::Box::from_raw(ptr.cast()) })
    }

    /// Borrow the value C holds, inside a callback.
    ///
    /// # Safety
    ///
    /// `ptr` came from [`into_raw`](Self::into_raw) or
    /// [`into_raw_local`](Self::into_raw_local) of a `CPayload<T>`, and C does
    /// not release it before `'a` ends — typically, `'a` is one callback.
    #[inline]
    #[must_use]
    pub unsafe fn borrow<'a>(ptr: *mut c_void) -> &'a T {
        // SAFETY: the caller passes a live payload C has not released; C
        // never writes it, and Rust reaches it only shared.
        unsafe { &*ptr.cast::<T>() }
    }

    /// The `void (*)(void *)` C calls to release the value: it drops it and
    /// frees its memory. A panic in `T`'s drop aborts.
    #[inline]
    #[must_use]
    pub fn release_fn() -> unsafe extern "C" fn(*mut c_void) {
        unsafe extern "C" fn release<T>(ptr: *mut c_void) {
            if !ptr.is_null() {
                // SAFETY: C passes a pointer from `into_raw`, once.
                drop(unsafe { CPayload::<T>::from_raw(ptr) });
            }
        }
        release::<T>
    }

    /// [`release_fn`](Self::release_fn), with `T` inferred from a payload
    /// still in hand — the type of a closure cannot be named.
    #[inline]
    #[must_use]
    pub fn release_fn_of(_: &Self) -> unsafe extern "C" fn(*mut c_void) {
        Self::release_fn()
    }

    /// The trampoline calling a registered closure, for a C callback type
    /// taking its context first; as [`CClosure::fn_ctx_first`].
    #[inline]
    #[must_use]
    pub fn fn_ctx_first<P>(&self) -> P
    where
        Self: CtxFirst<P>,
    {
        <Self as CtxFirst<P>>::trampoline()
    }

    /// The trampoline calling a registered closure, for a C callback type
    /// taking its context last; as [`CClosure::fn_ctx_last`].
    #[inline]
    #[must_use]
    pub fn fn_ctx_last<P>(&self) -> P
    where
        Self: CtxLast<P>,
    {
        <Self as CtxLast<P>>::trampoline()
    }
}

#[cfg(feature = "alloc")]
impl<T> core::fmt::Debug for CPayload<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CPayload").finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// The trampolines, per arity
// ---------------------------------------------------------------------------

macro_rules! trampolines {
    ($($a:ident),*) => {
        impl<'a, F, R: Copy, $($a),*> CtxFirst<unsafe extern "C" fn(*mut c_void $(, $a)*) -> R>
            for CClosure<'a, F, R>
        where
            F: FnMut($($a),*) -> R,
        {
            #[inline]
            fn trampoline() -> unsafe extern "C" fn(*mut c_void $(, $a)*) -> R {
                #[allow(non_snake_case)]
                unsafe extern "C" fn tramp<F: FnMut($($a),*) -> R, R: Copy, $($a),*>(
                    ctx: *mut c_void $(, $a: $a)*
                ) -> R {
                    // SAFETY: C passes back the context of the live
                    // `CClosure` this trampoline came with.
                    unsafe { invoke_scoped::<F, R>(ctx, |f| f($($a),*)) }
                }
                tramp::<F, R, $($a),*>
            }
        }

        impl<'a, F, R: Copy, $($a),*> CtxLast<unsafe extern "C" fn($($a,)* *mut c_void) -> R>
            for CClosure<'a, F, R>
        where
            F: FnMut($($a),*) -> R,
        {
            #[inline]
            fn trampoline() -> unsafe extern "C" fn($($a,)* *mut c_void) -> R {
                #[allow(non_snake_case)]
                unsafe extern "C" fn tramp<F: FnMut($($a),*) -> R, R: Copy, $($a),*>(
                    $($a: $a,)* ctx: *mut c_void
                ) -> R {
                    // SAFETY: as the context-first trampoline.
                    unsafe { invoke_scoped::<F, R>(ctx, |f| f($($a),*)) }
                }
                tramp::<F, R, $($a),*>
            }
        }

        #[cfg(feature = "alloc")]
        impl<F, R, $($a),*> CtxFirst<unsafe extern "C" fn(*mut c_void $(, $a)*) -> R>
            for CPayload<F>
        where
            F: Fn($($a),*) -> R,
        {
            #[inline]
            fn trampoline() -> unsafe extern "C" fn(*mut c_void $(, $a)*) -> R {
                #[allow(non_snake_case)]
                unsafe extern "C" fn tramp<F: Fn($($a),*) -> R, R, $($a),*>(
                    ctx: *mut c_void $(, $a: $a)*
                ) -> R {
                    // SAFETY: C passes back the payload it holds, unreleased.
                    let f = unsafe { CPayload::<F>::borrow(ctx) };
                    f($($a),*)
                }
                tramp::<F, R, $($a),*>
            }
        }

        #[cfg(feature = "alloc")]
        impl<F, R, $($a),*> CtxLast<unsafe extern "C" fn($($a,)* *mut c_void) -> R>
            for CPayload<F>
        where
            F: Fn($($a),*) -> R,
        {
            #[inline]
            fn trampoline() -> unsafe extern "C" fn($($a,)* *mut c_void) -> R {
                #[allow(non_snake_case)]
                unsafe extern "C" fn tramp<F: Fn($($a),*) -> R, R, $($a),*>(
                    $($a: $a,)* ctx: *mut c_void
                ) -> R {
                    // SAFETY: as the context-first trampoline.
                    let f = unsafe { CPayload::<F>::borrow(ctx) };
                    f($($a),*)
                }
                tramp::<F, R, $($a),*>
            }
        }
    };
}

trampolines!();
trampolines!(A);
trampolines!(A, B);
trampolines!(A, B, C);
trampolines!(A, B, C, D);
trampolines!(A, B, C, D, E);
trampolines!(A, B, C, D, E, G);
