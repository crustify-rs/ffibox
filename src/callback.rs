//! Callbacks across the boundary: Rust closures C calls, behind a C function
//! pointer and its `void *` context, C functions with their context that
//! Rust calls, the types that hold either, and the panic barrier.
//!
//! A C API takes a callback as a function pointer and a context pointer it
//! passes back on every call, as one of the callback's arguments: first
//! (`fn(void *ctx, …)`), last (`fn(…, void *ctx)`), or anywhere between. Two
//! lifetimes cover the APIs that do:
//!
//! | C keeps the callback | Primitive | Rust counterpart |
//! |----------------------|-----------|------------------|
//! | for one call (`foreach`, `qsort_r`) | [`CClosure`] | `&mut F` passed down |
//! | after the call returns, until it releases it (`set_log_callback`) | [`CPayload`] | `Box<F>` handed over |
//!
//! Each gives the trampoline, an `unsafe extern "C" fn` with the C type's
//! signature that recovers the closure from the context and calls it, and the
//! context itself. The trampoline's type is the one the C declaration names,
//! and so is where it takes the context: both are inferred from where the
//! trampoline is passed, and a closure of the wrong arity or argument types
//! does not compile:
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
//! let rc = unsafe { foreach(xs.as_ptr(), xs.len(), Some(cb.trampoline()), cb.ctx()) };
//! cb.finish();
//! assert_eq!((rc, sum), (0, 6));
//! ```
//!
//! Only the C API knows which argument is its context, and when its callback
//! takes more than one `void *`, its signature does not say. The position
//! then fails to infer, and is named, counting from 0, as in
//! `cb.trampoline::<_, 2>()` for a `(const void *key, void *value, void *ctx)`
//! callback ([`CTrampoline`]).
//!
//! The closure's arguments are the C types: the binding converts them to
//! handles and calls its consumer's closure with those, so the consumer's
//! closure takes safe types only.
//!
//! The other way, C hands Rust a function and its context to call: in a
//! struct field, as an argument, or through an export a ported module offers
//! the C that still calls it. [`CFn`] holds the pair for the call or borrow it
//! came with, and [`CFnBox`] holds it with the routine that releases the
//! context. Where one callable may be either, a consumer's closure or a C
//! function, [`CCallback`] and [`CCallbackBox`] hold whichever it is, and are
//! called from Rust and handed to C alike:
//!
//! | C keeps the callback | Rust closure | C function | Either |
//! |----------------------|--------------|------------|--------|
//! | for one call, or a borrow | [`CClosure`] | [`CFn`] | [`CCallback`] |
//! | until it releases it | [`CPayload`] | [`CFnBox`] | [`CCallbackBox`] |

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
// The trampolines
// ---------------------------------------------------------------------------

/// The C function pointer types a callback holder offers: `P`, with the
/// context at argument `CTX`, counting from 0.
///
/// Implemented for every C callback of up to 6 arguments besides the
/// context, `unsafe extern "C" fn(A, …, *mut c_void, …, G) -> R`, with the
/// context at each position, on [`CClosure`] and [`CPayload`]. Reached
/// through their `trampoline` method, where both `P` and `CTX` are inferred
/// from the C type the trampoline is passed as:
///
/// - the position is inferred when one argument alone is a `*mut c_void`, the
///   context, as in `fn(int x, void *ctx)`;
/// - it is named when several are, as in
///   `fn(const void *key, void *value, void *ctx)`, where the compiler cannot
///   tell the context from the others and the C API's documentation can:
///   `trampoline::<_, 2>()`.
///
/// ```compile_fail,E0284
/// use core::ffi::c_void;
/// use ffibox::callback::CClosure;
///
/// let mut f = |_: *const c_void, _: *mut c_void| {};
/// let cb = CClosure::new(&mut f, ());
/// // Which `void *` is the context? Not inferred: named as `::<_, 2>`.
/// let _: unsafe extern "C" fn(*const c_void, *mut c_void, *mut c_void) = cb.trampoline();
/// ```
pub trait CTrampoline<P, const CTX: usize> {
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
/// [`trampoline`](Self::trampoline) the trampoline, typed as the C
/// declaration names it. After the call,
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

    /// The trampoline for the C callback type `P`, with the context at
    /// argument `CTX`; both are inferred from where it is passed, and
    /// `CTX` is named when the C type cannot tell (see [`CTrampoline`]).
    #[inline]
    #[must_use]
    pub fn trampoline<P, const CTX: usize>(&self) -> P
    where
        Self: CTrampoline<P, CTX>,
    {
        <Self as CTrampoline<P, CTX>>::trampoline()
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
/// callback. For a closure, [`trampoline`](Self::trampoline) gives the
/// trampoline that calls it.
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
///     cb: Some(payload.trampoline()),
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

    /// The trampoline calling a registered closure, for the C callback type
    /// `P` with the context at argument `CTX`; as [`CClosure::trampoline`].
    #[inline]
    #[must_use]
    pub fn trampoline<P, const CTX: usize>(&self) -> P
    where
        Self: CTrampoline<P, CTX>,
    {
        <Self as CTrampoline<P, CTX>>::trampoline()
    }
}

/// Take a box as the payload, without moving the value.
#[cfg(feature = "alloc")]
impl<T> From<alloc::boxed::Box<T>> for CPayload<T> {
    #[inline]
    fn from(b: alloc::boxed::Box<T>) -> Self {
        Self(b)
    }
}

#[cfg(feature = "alloc")]
impl<T> CPayload<T> {
    /// The context pointer to the value, without surrendering it: for a C
    /// call that uses it while `self` is borrowed and keeps nothing.
    #[inline]
    #[must_use]
    pub fn ctx(&self) -> *mut c_void {
        core::ptr::from_ref::<T>(&self.0).cast_mut().cast()
    }

    /// The value, back in a box, as before it was handed to C.
    #[inline]
    #[must_use]
    pub fn into_box(self) -> alloc::boxed::Box<T> {
        self.0
    }
}

#[cfg(feature = "alloc")]
impl<T> core::fmt::Debug for CPayload<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CPayload").finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// CFn — a C function and its context, which Rust calls
// ---------------------------------------------------------------------------

/// The argument tuples a C function and its context can be called with:
/// `(A, …)` for `P`, the context left out. Implemented with
/// [`CTrampoline`]'s shapes, and reached through [`CFn::call`].
pub trait CFnCall<Args> {
    /// What the C function returns.
    type Output;

    /// Call the function with `args`, the context at its position.
    ///
    /// # Safety
    ///
    /// As [`CFn::call`].
    #[doc(hidden)]
    unsafe fn call(&self, args: Args) -> Self::Output;
}

/// A C function and the context it takes, which Rust calls: the C half of a
/// callback, as [`CClosure`] is the Rust half. `P` is the C function pointer
/// type, `unsafe extern "C" fn(…) -> R`, and `CTX` the context's argument,
/// counting from 0, as [`CTrampoline`] counts it.
///
/// A C API hands Rust a callback, in a struct field, as a function's
/// argument, or through an export a ported module offers to the C that still
/// calls it, and Rust calls it later. The pair stays valid for `'a`, which
/// the binding chooses from the C API's contract: the call it came with, or
/// the borrow of the object holding it.
///
/// Built with an `unsafe` [`new`](Self::new), whose contract is what the C
/// that supplies the pair guarantees, and called with an `unsafe`
/// [`call`](Self::call), whose arguments are C's types: the binding's safe
/// callable converts its own arguments to them and calls in one justified
/// block. [`parts`](Self::parts) hands the pair back to C unchanged, so a
/// callback C supplied reaches another C API with no trampoline between.
///
/// Neither `Send` nor `Sync`: it is called on the thread that holds it.
// TODO(CFn threads): a C function that the C API lets Rust call from other
// threads, once a campaign needs it.
pub struct CFn<'a, P, const CTX: usize> {
    f: P,
    ctx: *mut c_void,
    _valid: PhantomData<&'a ()>,
}

impl<'a, P: Copy, const CTX: usize> CFn<'a, P, CTX> {
    /// Take a C function and its context.
    ///
    /// # Safety
    ///
    /// `f` is a C function of type `P` that may be called with `ctx` as its
    /// argument `CTX`, any number of times, on the calling thread, until `'a`
    /// ends.
    #[inline]
    pub unsafe fn new(f: P, ctx: *mut c_void) -> Self {
        Self {
            f,
            ctx,
            _valid: PhantomData,
        }
    }

    /// The function and its context, to hand to C.
    #[inline]
    #[must_use]
    pub fn parts(&self) -> (P, *mut c_void) {
        (self.f, self.ctx)
    }

    /// Call the function with `args`, the C arguments other than the
    /// context, in order: `cb.call((item,))` for `fn(item, ctx)`.
    ///
    /// # Safety
    ///
    /// `args` satisfy what the C callback type requires of its arguments, as
    /// its C API documents them.
    #[inline]
    pub unsafe fn call<Args>(&self, args: Args) -> <Self as CFnCall<Args>>::Output
    where
        Self: CFnCall<Args>,
    {
        // SAFETY: the caller upholds the arguments' contract; `new`'s
        // contract makes the function and its context callable now.
        unsafe { <Self as CFnCall<Args>>::call(self, args) }
    }
}

impl<P: Copy, const CTX: usize> Clone for CFn<'_, P, CTX> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}
// A copy is the same pair, valid as long; `#[derive]` would bound `P` only.
impl<P: Copy, const CTX: usize> Copy for CFn<'_, P, CTX> {}

impl<P, const CTX: usize> core::fmt::Debug for CFn<'_, P, CTX> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CFn")
            .field("ctx", &self.ctx)
            .finish_non_exhaustive()
    }
}

/// A C function and the context it takes, owned: C's counterpart of
/// [`CPayload`], for a callback C hands over with the routine that releases
/// its context (`set_cb(cb, ctx, free)`). Dropping it calls the release on
/// the context; [`into_raw`](Self::into_raw) gives all three back to C.
///
/// Neither `Send` nor `Sync`, as [`CFn`].
#[must_use = "dropping a CFnBox releases its context"]
pub struct CFnBox<P, const CTX: usize> {
    f: P,
    ctx: *mut c_void,
    release: Option<unsafe extern "C" fn(*mut c_void)>,
}

impl<P: Copy, const CTX: usize> CFnBox<P, CTX> {
    /// Take a C function, its context and the routine that releases the
    /// context.
    ///
    /// # Safety
    ///
    /// As [`CFn::new`], until the box is dropped or surrendered; and
    /// `release`, when given, may be called once with `ctx`, after which
    /// nothing calls `f` with it.
    #[inline]
    pub unsafe fn new(
        f: P,
        ctx: *mut c_void,
        release: Option<unsafe extern "C" fn(*mut c_void)>,
    ) -> Self {
        Self { f, ctx, release }
    }

    /// The pair, borrowed: callable, or handed to a C call, while the box
    /// lives.
    #[inline]
    #[must_use]
    pub fn as_fn(&self) -> CFn<'_, P, CTX> {
        // SAFETY: `new`'s contract makes the pair callable until the box is
        // dropped, which the borrow outlasts.
        unsafe { CFn::new(self.f, self.ctx) }
    }

    /// Call the function, as [`CFn::call`].
    ///
    /// # Safety
    ///
    /// As [`CFn::call`].
    #[inline]
    pub unsafe fn call<Args, R>(&self, args: Args) -> R
    where
        for<'b> CFn<'b, P, CTX>: CFnCall<Args, Output = R>,
    {
        // SAFETY: as the caller's.
        unsafe { self.as_fn().call(args) }
    }

    /// Give the function, its context and its release back to C, without
    /// releasing.
    #[inline]
    #[must_use = "the context must be released"]
    pub fn into_raw(self) -> (P, *mut c_void, Option<unsafe extern "C" fn(*mut c_void)>) {
        let b = core::mem::ManuallyDrop::new(self);
        (b.f, b.ctx, b.release)
    }
}

impl<P, const CTX: usize> Drop for CFnBox<P, CTX> {
    fn drop(&mut self) {
        if let Some(release) = self.release {
            // SAFETY: `new`'s contract: called once, with the context.
            unsafe { release(self.ctx) }
        }
    }
}

impl<P, const CTX: usize> core::fmt::Debug for CFnBox<P, CTX> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CFnBox")
            .field("ctx", &self.ctx)
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// CCallback — either half, called and handed to C alike
// ---------------------------------------------------------------------------

/// A callback that is either a Rust closure ([`CClosure`]) or a C function
/// with its context ([`CFn`]), used the same way whichever it is: handed to C
/// as a function and context ([`c_form`](Self::c_form)), or called from Rust
/// ([`call`](Self::call)).
///
/// What a binding's callable type holds where C keeps the callback for the
/// call it receives, `'a`: a consumer's closure, or a callback C supplied that
/// ported code now calls. Its consumers never see which.
///
/// Called from Rust, a closure runs through its trampoline, as when C calls
/// it: a re-entrant call is refused and a panic caught, with the closure's
/// failure value, and [`finish`](Self::finish) resumes the panic.
#[must_use = "a CCallback does nothing until it is called"]
pub struct CCallback<'a, P, F, R: Copy, const CTX: usize>(
    Either<CClosure<'a, F, R>, CFn<'a, P, CTX>>,
);

/// The two halves of a callback.
enum Either<L, R> {
    Rust(L),
    C(R),
}

impl<'a, P: Copy, F, R: Copy, const CTX: usize> CCallback<'a, P, F, R, CTX> {
    /// A Rust closure, as [`CClosure::new`].
    #[inline]
    pub fn new(f: &'a mut F, on_failure: R) -> Self
    where
        F: Send,
    {
        Self(Either::Rust(CClosure::new(f, on_failure)))
    }

    /// A Rust closure, as [`CClosure::new_local`].
    ///
    /// # Safety
    ///
    /// As [`CClosure::new_local`]: C calls it on the calling thread only.
    #[inline]
    pub unsafe fn new_local(f: &'a mut F, on_failure: R) -> Self {
        // SAFETY: as the caller's.
        Self(Either::Rust(unsafe { CClosure::new_local(f, on_failure) }))
    }

    /// A C function and its context.
    #[inline]
    pub fn from_c(f: CFn<'a, P, CTX>) -> Self {
        Self(Either::C(f))
    }

    /// The function and context to hand to C: the closure's trampoline and
    /// context, or the C function's own pair. Valid while `self` is borrowed.
    #[inline]
    #[must_use]
    pub fn c_form(&self) -> (P, *mut c_void)
    where
        CClosure<'a, F, R>: CTrampoline<P, CTX>,
    {
        match &self.0 {
            Either::Rust(c) => (c.trampoline(), c.ctx()),
            Either::C(f) => f.parts(),
        }
    }

    /// Call the callback with `args`, the C arguments other than the
    /// context, as [`CFn::call`].
    ///
    /// # Safety
    ///
    /// As [`CFn::call`]: `args` satisfy the C callback type's contract.
    #[inline]
    pub unsafe fn call<Args>(&self, args: Args) -> R
    where
        CClosure<'a, F, R>: CTrampoline<P, CTX>,
        for<'b> CFn<'b, P, CTX>: CFnCall<Args, Output = R>,
    {
        let (f, ctx) = self.c_form();
        // SAFETY: a closure's trampoline and context are callable while
        // `self` is borrowed, and a C pair is, per its `CFn`, for `'a`.
        let f: CFn<'_, P, CTX> = unsafe { CFn::new(f, ctx) };
        // SAFETY: as the caller's.
        unsafe { f.call(args) }
    }

    /// End the callback's use, resuming a panic its closure raised, as
    /// [`CClosure::finish`].
    #[inline]
    pub fn finish(self) {
        drop(self);
    }
}

impl<P, F, R: Copy, const CTX: usize> core::fmt::Debug for CCallback<'_, P, F, R, CTX> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match &self.0 {
            Either::Rust(c) => f.debug_tuple("CCallback").field(c).finish(),
            Either::C(c) => f.debug_tuple("CCallback").field(c).finish(),
        }
    }
}

/// A callback that C keeps until it releases it, either a Rust value
/// ([`CPayload`]) or a C function with its context and release
/// ([`CFnBox`]): [`CCallback`]'s owned counterpart, for a binding's callable
/// type that C stores, or that ported code stores.
///
/// Called from Rust, a Rust closure runs through its trampoline, as when C
/// calls it: shared, so it is an `Fn`, and a panic in it aborts.
#[cfg(feature = "alloc")]
#[cfg_attr(docsrs, doc(cfg(feature = "alloc")))]
#[must_use = "dropping a CCallbackBox releases its callback"]
pub struct CCallbackBox<P, F, const CTX: usize>(Either<CPayload<F>, CFnBox<P, CTX>>);

#[cfg(feature = "alloc")]
impl<P: Copy, F, const CTX: usize> CCallbackBox<P, F, CTX> {
    /// A Rust closure or value, as [`CPayload::new`].
    #[inline]
    pub fn new(f: F) -> Self {
        Self(Either::Rust(CPayload::new(f)))
    }

    /// A C function with its context and release.
    #[inline]
    pub fn from_c(f: CFnBox<P, CTX>) -> Self {
        Self(Either::C(f))
    }

    /// The function and context to hand to a C call that uses them while
    /// `self` is borrowed, and keeps neither.
    #[inline]
    #[must_use]
    pub fn c_form(&self) -> (P, *mut c_void)
    where
        CPayload<F>: CTrampoline<P, CTX>,
    {
        match &self.0 {
            Either::Rust(p) => (p.trampoline(), p.ctx()),
            Either::C(f) => f.as_fn().parts(),
        }
    }

    /// Surrender the callback to C, which keeps it: the function, the
    /// context, and the routine C calls once to release the context.
    #[inline]
    #[must_use = "the context must be released"]
    pub fn into_c(self) -> (P, *mut c_void, Option<unsafe extern "C" fn(*mut c_void)>)
    where
        CPayload<F>: CTrampoline<P, CTX>,
        F: Send + Sync,
    {
        match self.0 {
            Either::Rust(p) => {
                let (f, release) = (p.trampoline(), CPayload::release_fn_of(&p));
                (f, p.into_raw(), Some(release))
            }
            Either::C(f) => f.into_raw(),
        }
    }

    /// Call the callback with `args`, as [`CFn::call`].
    ///
    /// # Safety
    ///
    /// As [`CFn::call`].
    #[inline]
    pub unsafe fn call<Args, R>(&self, args: Args) -> R
    where
        CPayload<F>: CTrampoline<P, CTX>,
        for<'b> CFn<'b, P, CTX>: CFnCall<Args, Output = R>,
    {
        let (f, ctx) = self.c_form();
        // SAFETY: a payload's trampoline and context are callable while
        // `self` is borrowed, and a `CFnBox`'s pair while it lives.
        let f: CFn<'_, P, CTX> = unsafe { CFn::new(f, ctx) };
        // SAFETY: as the caller's.
        unsafe { f.call(args) }
    }
}

#[cfg(feature = "alloc")]
impl<P, F, const CTX: usize> core::fmt::Debug for CCallbackBox<P, F, CTX> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match &self.0 {
            Either::Rust(p) => f.debug_tuple("CCallbackBox").field(p).finish(),
            Either::C(c) => f.debug_tuple("CCallbackBox").field(c).finish(),
        }
    }
}

// ---------------------------------------------------------------------------
// The trampolines, per arity and context position
// ---------------------------------------------------------------------------

/// `1` per argument, to count the arguments before the context.
macro_rules! one {
    ($x:ident) => {
        1
    };
}

/// The trampolines for one C callback shape: the arguments `$b` before the
/// context and `$a` after it.
macro_rules! trampoline_at {
    ([$($b:ident),*] [$($a:ident),*]) => {
        impl<'a, F, R: Copy, $($b,)* $($a),*>
            CTrampoline<unsafe extern "C" fn($($b,)* *mut c_void $(, $a)*) -> R, { 0 $(+ one!($b))* }>
            for CClosure<'a, F, R>
        where
            F: FnMut($($b,)* $($a),*) -> R,
        {
            #[inline]
            fn trampoline() -> unsafe extern "C" fn($($b,)* *mut c_void $(, $a)*) -> R {
                #[allow(non_snake_case)]
                unsafe extern "C" fn tramp<F: FnMut($($b,)* $($a),*) -> R, R: Copy, $($b,)* $($a),*>(
                    $($b: $b,)* ctx: *mut c_void $(, $a: $a)*
                ) -> R {
                    // SAFETY: C passes back the context of the live
                    // `CClosure` this trampoline came with.
                    unsafe { invoke_scoped::<F, R>(ctx, |f| f($($b,)* $($a),*)) }
                }
                tramp::<F, R, $($b,)* $($a),*>
            }
        }

        #[cfg(feature = "alloc")]
        impl<F, R, $($b,)* $($a),*>
            CTrampoline<unsafe extern "C" fn($($b,)* *mut c_void $(, $a)*) -> R, { 0 $(+ one!($b))* }>
            for CPayload<F>
        where
            F: Fn($($b,)* $($a),*) -> R,
        {
            #[inline]
            fn trampoline() -> unsafe extern "C" fn($($b,)* *mut c_void $(, $a)*) -> R {
                #[allow(non_snake_case)]
                unsafe extern "C" fn tramp<F: Fn($($b,)* $($a),*) -> R, R, $($b,)* $($a),*>(
                    $($b: $b,)* ctx: *mut c_void $(, $a: $a)*
                ) -> R {
                    // SAFETY: C passes back the payload it holds, unreleased.
                    let f = unsafe { CPayload::<F>::borrow(ctx) };
                    f($($b,)* $($a),*)
                }
                tramp::<F, R, $($b,)* $($a),*>
            }
        }

        impl<'a, R, $($b,)* $($a),*> CFnCall<($($b,)* $($a,)*)>
            for CFn<'a, unsafe extern "C" fn($($b,)* *mut c_void $(, $a)*) -> R, { 0 $(+ one!($b))* }>
        {
            type Output = R;

            #[inline]
            #[allow(non_snake_case)]
            unsafe fn call(&self, ($($b,)* $($a,)*): ($($b,)* $($a,)*)) -> R {
                // SAFETY: as `CFn::call`'s caller's, with the context at the
                // position the type names.
                unsafe { (self.f)($($b,)* self.ctx $(, $a)*) }
            }
        }
    };
}

/// The trampolines for a callback of the arguments `$a`, with the context at
/// each position: before the first, then after each in turn.
macro_rules! trampolines {
    ($($a:ident),*) => {
        trampolines!(@at [] [$($a),*]);
    };
    (@at [$($b:ident),*] []) => {
        trampoline_at!([$($b),*] []);
    };
    (@at [$($b:ident),*] [$h:ident $(, $t:ident)*]) => {
        trampoline_at!([$($b),*] [$h $(, $t)*]);
        trampolines!(@at [$($b,)* $h] [$($t),*]);
    };
}

trampolines!();
trampolines!(A);
trampolines!(A, B);
trampolines!(A, B, C);
trampolines!(A, B, C, D);
trampolines!(A, B, C, D, E);
trampolines!(A, B, C, D, E, G);
