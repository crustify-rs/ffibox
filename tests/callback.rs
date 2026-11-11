//! Rust code C calls: `CClosure` for a callback C uses during one call,
//! `CPayload` for one it keeps, and the panic barrier.

#![allow(missing_docs)]

use core::cell::Cell;
use core::ffi::{c_int, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use ffibox::callback::{CClosure, CPayload, barrier};

type EachLast = unsafe extern "C" fn(c_int, *mut c_void) -> c_int;
type EachFirst = unsafe extern "C" fn(*mut c_void, c_int, c_int, c_int) -> c_int;

/// Stands in for a C routine calling `cb(x, ctx)` for each element until one
/// returns non-zero.
unsafe extern "C" fn foreach(
    xs: *const c_int,
    n: usize,
    cb: Option<EachLast>,
    ctx: *mut c_void,
) -> c_int {
    for i in 0..n {
        // SAFETY: `xs` has `n` elements; the callback is the caller's.
        let rc = unsafe { cb.unwrap()(*xs.add(i), ctx) };
        if rc != 0 {
            return rc;
        }
    }
    0
}

/// Stands in for a C routine calling a context-first, three-argument
/// callback once.
unsafe extern "C" fn call3(cb: EachFirst, ctx: *mut c_void) -> c_int {
    // SAFETY: the callback is the caller's.
    unsafe { cb(ctx, 1, 2, 3) }
}

#[test]
fn a_closure_changes_local_state_through_a_context_last_callback() {
    let xs = [1, 2, 3, 4];
    let mut seen = Vec::new();
    let mut f = |x: c_int| {
        seen.push(x);
        c_int::from(x == 3)
    };
    let cb = CClosure::new(&mut f, -1);
    // SAFETY: `foreach` calls back during the call only.
    let rc = unsafe { foreach(xs.as_ptr(), xs.len(), Some(cb.trampoline()), cb.ctx()) };
    assert!(!cb.failed());
    cb.finish();
    assert_eq!(rc, 1, "the closure's stop value is C's return");
    assert_eq!(seen, [1, 2, 3]);
}

#[test]
fn a_context_first_callback_of_three_arguments() {
    let mut f = |a: c_int, b: c_int, c: c_int| a * 100 + b * 10 + c;
    let cb = CClosure::new(&mut f, -1);
    // SAFETY: `call3` calls back during the call only.
    assert_eq!(unsafe { call3(cb.trampoline(), cb.ctx()) }, 123);
}

#[test]
fn a_callback_without_arguments() {
    let mut n = 0;
    let mut f = || n += 1;
    let cb = CClosure::new(&mut f, ());
    let tramp: unsafe extern "C" fn(*mut c_void) = cb.trampoline();
    // SAFETY: the context of the live `cb`.
    unsafe {
        tramp(cb.ctx());
        tramp(cb.ctx());
    }
    cb.finish();
    assert_eq!(n, 2);
}

#[test]
fn a_panic_stops_the_routine_and_resumes_after_it() {
    let xs = [1, 2, 3, 4];
    let calls = Cell::new(0);
    let mut f = |x: c_int| {
        calls.set(calls.get() + 1);
        assert_ne!(x, 2, "boom at 2");
        0
    };
    let cb = CClosure::new_local_for_test(&mut f);
    // SAFETY: `foreach` calls back during the call, on this thread.
    let rc = unsafe { foreach(xs.as_ptr(), xs.len(), Some(cb.trampoline()), cb.ctx()) };
    assert_eq!(rc, -1, "the failure value stopped the routine");
    assert!(cb.failed());
    let resumed = catch_unwind(AssertUnwindSafe(|| cb.finish()));
    let msg = resumed.unwrap_err();
    assert!(msg.downcast_ref::<String>().unwrap().contains("boom at 2"));
    assert_eq!(calls.get(), 2, "not called again after the panic");
}

/// `CClosure::new_local` for the tests: the C stand-ins call back on the
/// calling thread.
trait NewLocal<'a, F> {
    fn new_local_for_test(f: &'a mut F) -> Self;
}
impl<'a, F> NewLocal<'a, F> for CClosure<'a, F, c_int> {
    fn new_local_for_test(f: &'a mut F) -> Self {
        // SAFETY: every routine these tests pass it to calls back on the
        // calling thread.
        unsafe { CClosure::new_local(f, -1) }
    }
}

#[test]
fn a_reentrant_invocation_is_refused_and_reported() {
    // The closure re-enters itself through the trampoline, as a nested C call
    // re-invoking the callback would; a second `&mut F` must not be made.
    let ctx = Cell::new(core::ptr::null_mut::<c_void>());
    let tramp: Cell<Option<EachLast>> = Cell::new(None);
    let inner = Cell::new(0);
    let mut f = |x: c_int| {
        if x == 1 {
            // SAFETY: the live context; the call is refused.
            inner.set(unsafe { tramp.get().unwrap()(2, ctx.get()) });
        }
        0
    };
    let cb = CClosure::new_local_for_test(&mut f);
    ctx.set(cb.ctx());
    tramp.set(Some(cb.trampoline()));
    let xs = [1];
    // SAFETY: `foreach` calls back during the call, on this thread.
    let rc = unsafe { foreach(xs.as_ptr(), 1, tramp.get(), ctx.get()) };
    assert_eq!(
        (rc, inner.get()),
        (0, -1),
        "the nested call got the failure value"
    );
    assert!(cb.failed());
    let r = catch_unwind(AssertUnwindSafe(|| cb.finish()));
    assert!(r.is_err(), "finish reports the refused invocation");
}

#[test]
fn a_send_closure_may_be_called_back_on_another_thread() {
    let mut total = 0;
    let mut f = |x: c_int| {
        total += x;
        0
    };
    let cb = CClosure::new(&mut f, -1);
    let tramp: EachLast = cb.trampoline();
    /// The context, sent to the worker as C would pass it: by pointer.
    struct Ctx(*mut c_void);
    // SAFETY: the closure is `Send`, and one thread uses the context at a time.
    unsafe impl Send for Ctx {}
    let ctx = Ctx(cb.ctx());
    // Stands in for a C routine that calls back from a worker it joins.
    std::thread::scope(|s| {
        s.spawn(move || {
            let ctx = ctx;
            for x in [5, 6] {
                // SAFETY: the live context, used by one thread at a time.
                unsafe { tramp(x, ctx.0) };
            }
        });
    });
    cb.finish();
    assert_eq!(total, 11);
}

#[test]
fn a_payload_is_called_back_then_released_once() {
    struct Lib {
        cb: Option<unsafe extern "C" fn(c_int, *mut c_void) -> c_int>,
        ctx: *mut c_void,
        free: Option<unsafe extern "C" fn(*mut c_void)>,
    }
    static DROPS: AtomicUsize = AtomicUsize::new(0);
    struct Counted(Arc<AtomicUsize>);
    impl Drop for Counted {
        fn drop(&mut self) {
            DROPS.fetch_add(1, Ordering::SeqCst);
        }
    }
    let sum = Arc::new(AtomicUsize::new(0));
    let guard = Counted(sum.clone());
    let payload = CPayload::new(move |x: c_int| {
        guard.0.fetch_add(x as usize, Ordering::SeqCst);
        0
    });
    let mut lib = Lib {
        cb: Some(payload.trampoline()),
        free: Some(CPayload::release_fn_of(&payload)),
        ctx: payload.into_raw(),
    };
    for x in [1, 2, 3] {
        // SAFETY: the payload is live until `free`.
        unsafe { lib.cb.unwrap()(x, lib.ctx) };
    }
    assert_eq!(sum.load(Ordering::SeqCst), 6);
    assert_eq!(DROPS.load(Ordering::SeqCst), 0);
    // SAFETY: released once.
    unsafe { lib.free.take().unwrap()(lib.ctx) };
    assert_eq!(DROPS.load(Ordering::SeqCst), 1);
    // SAFETY: NULL is a no-op.
    unsafe { CPayload::<u8>::release_fn()(core::ptr::null_mut()) };
}

#[test]
fn a_payload_is_borrowed_in_a_callback_and_taken_back() {
    let raw = CPayload::new(String::from("ctx")).into_raw();
    // SAFETY: a live payload of `String`.
    assert_eq!(unsafe { CPayload::<String>::borrow(raw) }, "ctx");
    // SAFETY: taken back once, C no longer holding it.
    let back = unsafe { CPayload::<String>::from_raw(raw) };
    drop(back);
}

#[test]
fn a_local_payload_may_hold_non_send_state() {
    let hits = std::rc::Rc::new(Cell::new(0));
    let h = hits.clone();
    let payload = CPayload::new(move || h.set(h.get() + 1));
    let tramp: unsafe extern "C" fn(*mut c_void) = payload.trampoline();
    let free = CPayload::release_fn_of(&payload);
    // SAFETY: called back and released on this thread.
    let ctx = unsafe { payload.into_raw_local() };
    // SAFETY: live until `free`.
    unsafe {
        tramp(ctx);
        tramp(ctx);
        free(ctx);
    }
    assert_eq!((hits.get(), std::rc::Rc::strong_count(&hits)), (2, 1));
}

#[test]
fn the_barrier_maps_a_panic_to_the_error_value() {
    assert_eq!(barrier(|| 3, || -1), 3);
    assert_eq!(barrier(|| -> i32 { panic!("x") }, || -1), -1);
}

#[test]
fn a_context_in_the_middle_is_inferred() {
    /// Stands in for `int visit(cb_t cb, void *ctx)`, calling
    /// `cb(name, ctx, flags)`.
    type Mid = unsafe extern "C" fn(*const u8, *mut c_void, c_int) -> c_int;
    unsafe extern "C" fn visit(cb: Mid, ctx: *mut c_void) -> c_int {
        // SAFETY: the callback is the caller's.
        unsafe { cb(c"x".as_ptr().cast(), ctx, 7) }
    }
    let mut f = |name: *const u8, flags: c_int| {
        // SAFETY: `visit` passes a NUL-terminated name.
        let first = unsafe { *name };
        c_int::from(first) + flags
    };
    let cb = CClosure::new(&mut f, -1);
    // SAFETY: `visit` calls back during the call only.
    let rc = unsafe { visit(cb.trampoline(), cb.ctx()) };
    assert_eq!(rc, i32::from(b'x') + 7);
}

#[test]
fn a_context_among_other_void_pointers_is_named() {
    /// Stands in for a hash-table foreach calling `cb(key, value, ctx)`,
    /// all three `void *`: the position is the C API's to say.
    type Each = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> c_int;
    unsafe extern "C" fn each(cb: Each, ctx: *mut c_void) -> c_int {
        let (mut k, mut v) = (1u8, 2u8);
        // SAFETY: the callback is the caller's.
        unsafe { cb((&raw mut k).cast(), (&raw mut v).cast(), ctx) }
    }
    let mut f = |k: *mut c_void, v: *mut c_void| {
        // SAFETY: `each` passes a live key and value.
        unsafe { c_int::from(*k.cast::<u8>()) * 10 + c_int::from(*v.cast::<u8>()) }
    };
    let cb = CClosure::new(&mut f, -1);
    // SAFETY: `each` calls back during the call only, with `ctx` third.
    assert_eq!(unsafe { each(cb.trampoline::<_, 2>(), cb.ctx()) }, 12);
}

#[test]
fn every_position_of_the_widest_shape() {
    type W0 = unsafe extern "C" fn(*mut c_void, u8, u8, u8, u8, u8, u8) -> u32;
    type W3 = unsafe extern "C" fn(u8, u8, u8, *mut c_void, u8, u8, u8) -> u32;
    type W6 = unsafe extern "C" fn(u8, u8, u8, u8, u8, u8, *mut c_void) -> u32;
    let mut f = |a: u8, b: u8, c: u8, d: u8, e: u8, g: u8| {
        [a, b, c, d, e, g]
            .iter()
            .fold(0u32, |n, &x| n * 10 + u32::from(x))
    };
    let cb = CClosure::new(&mut f, 0);
    let (w0, w3, w6): (W0, W3, W6) = (cb.trampoline(), cb.trampoline(), cb.trampoline());
    let ctx = cb.ctx();
    // SAFETY: the context of the live `cb`, at the position each type names.
    unsafe {
        assert_eq!(w0(ctx, 1, 2, 3, 4, 5, 6), 123_456);
        assert_eq!(w3(1, 2, 3, ctx, 4, 5, 6), 123_456);
        assert_eq!(w6(1, 2, 3, 4, 5, 6, ctx), 123_456);
    }
}
