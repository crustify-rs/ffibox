//! A callback is either half: a Rust closure C calls, or a C function with
//! its context that Rust calls. `CFn` and `CFnBox` hold the C half;
//! `CCallback` and `CCallbackBox` hold either, called from Rust and handed to
//! C alike.

#![allow(missing_docs)]

use core::ffi::{c_int, c_void};
use std::cell::Cell;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};

use ffibox::{CCallback, CCallbackBox, CFn, CFnBox};

/// The C callback type: `int (*)(int x, void *ctx)`.
type Each = unsafe extern "C" fn(c_int, *mut c_void) -> c_int;

/// Stands in for a C routine calling `cb(x, ctx)` for each element until one
/// returns non-zero.
unsafe extern "C" fn foreach(xs: *const c_int, n: usize, cb: Each, ctx: *mut c_void) -> c_int {
    for i in 0..n {
        // SAFETY: `xs` has `n` elements; the callback is the caller's.
        let rc = unsafe { cb(*xs.add(i), ctx) };
        if rc != 0 {
            return rc;
        }
    }
    0
}

/// A C callback: adds `x` to the `c_int` its context points at.
unsafe extern "C" fn c_add(x: c_int, ctx: *mut c_void) -> c_int {
    // SAFETY: the context is a live `c_int` the caller lends.
    unsafe { *ctx.cast::<c_int>() += x };
    0
}

#[test]
fn rust_calls_a_c_function_with_its_context() {
    let mut sum: c_int = 0;
    // SAFETY: `c_add` may be called with `&mut sum` while it is borrowed.
    let f: CFn<'_, Each, 1> = unsafe { CFn::new(c_add, (&raw mut sum).cast()) };
    // SAFETY: `c_add` accepts any `int`.
    unsafe {
        assert_eq!(f.call((2,)), 0);
        assert_eq!(f.call((3,)), 0);
    }
    // The pair goes back to C unchanged, with no trampoline between.
    let (cb, ctx) = f.parts();
    let xs = [10, 20];
    // SAFETY: as above.
    unsafe { foreach(xs.as_ptr(), xs.len(), cb, ctx) };
    assert_eq!(sum, 35);
}

#[test]
fn the_context_is_passed_at_its_position() {
    type Mid = unsafe extern "C" fn(c_int, *mut c_void, c_int) -> c_int;
    unsafe extern "C" fn sub(a: c_int, ctx: *mut c_void, b: c_int) -> c_int {
        // SAFETY: the context is a live `c_int`.
        unsafe { *ctx.cast::<c_int>() + a - b }
    }
    let mut base: c_int = 100;
    // SAFETY: `sub` may be called with `&mut base` while it is borrowed.
    let f: CFn<'_, Mid, 1> = unsafe { CFn::new(sub, (&raw mut base).cast()) };
    // SAFETY: `sub` accepts any `int`s.
    assert_eq!(unsafe { f.call((7, 2)) }, 105);
}

static RELEASES: AtomicUsize = AtomicUsize::new(0);

/// A C context and its release: a heap `c_int` total.
fn c_total() -> *mut c_void {
    Box::into_raw(Box::new(0 as c_int)).cast()
}
unsafe extern "C" fn c_total_free(ctx: *mut c_void) {
    RELEASES.fetch_add(1, Ordering::SeqCst);
    // SAFETY: a context from `c_total`, released once.
    drop(unsafe { Box::from_raw(ctx.cast::<c_int>()) });
}

#[test]
fn an_owned_c_callback_releases_its_context_once() {
    let before = RELEASES.load(Ordering::SeqCst);
    let ctx = c_total();
    // SAFETY: `c_add` takes the `c_int` `c_total` made, which
    // `c_total_free` releases once.
    let f: CFnBox<Each, 1> = unsafe { CFnBox::new(c_add, ctx, Some(c_total_free)) };
    // SAFETY: `c_add` accepts any `int`.
    unsafe { f.call::<_, c_int>((5,)) };
    // SAFETY: the context is live until the box drops.
    assert_eq!(unsafe { *ctx.cast::<c_int>() }, 5);
    drop(f);
    assert_eq!(RELEASES.load(Ordering::SeqCst), before + 1);

    // Surrendered, it is not released: its new holder does that.
    // SAFETY: as above.
    let g: CFnBox<Each, 1> = unsafe { CFnBox::new(c_add, c_total(), Some(c_total_free)) };
    let (_, ctx, release) = g.into_raw();
    assert_eq!(RELEASES.load(Ordering::SeqCst), before + 1);
    // SAFETY: C's side: the context is released once.
    unsafe { release.unwrap()(ctx) };
    assert_eq!(RELEASES.load(Ordering::SeqCst), before + 2);
}

/// What a binding's code does with a callable it holds, whichever half it
/// is: call it from Rust, then hand it to a C routine.
fn use_both_ways<F>(cb: &CCallback<'_, Each, F, c_int, 1>) -> (c_int, c_int)
where
    F: FnMut(c_int) -> c_int,
{
    // SAFETY: the callback takes any `int`.
    let direct = unsafe { cb.call((1,)) };
    let (f, ctx) = cb.c_form();
    let xs = [2, 3];
    // SAFETY: `foreach` calls back during the call only.
    let via_c = unsafe { foreach(xs.as_ptr(), xs.len(), f, ctx) };
    (direct, via_c)
}

#[test]
fn a_callback_is_used_alike_whichever_half_it_is() {
    // A consumer's closure.
    let mut seen = Vec::new();
    let mut f = |x: c_int| {
        seen.push(x);
        0
    };
    let cb = CCallback::new(&mut f, -1);
    assert_eq!(use_both_ways(&cb), (0, 0));
    cb.finish();
    assert_eq!(seen, [1, 2, 3]);

    // A C function C supplied.
    let mut sum: c_int = 0;
    // SAFETY: `c_add` may be called with `&mut sum` while it is borrowed.
    let f = unsafe { CFn::new(c_add as Each, (&raw mut sum).cast()) };
    let cb = CCallback::<'_, Each, fn(c_int) -> c_int, c_int, 1>::from_c(f);
    assert_eq!(use_both_ways(&cb), (0, 0));
    drop(cb);
    assert_eq!(sum, 6);
}

#[test]
fn a_panicking_closure_called_from_rust_fails_then_resumes() {
    let mut f = |x: c_int| -> c_int {
        assert!(x < 2, "too big");
        0
    };
    let cb: CCallback<'_, Each, _, c_int, 1> = CCallback::new(&mut f, -1);
    // SAFETY: the callback takes any `int`.
    unsafe {
        assert_eq!(cb.call((1,)), 0);
        assert_eq!(cb.call((5,)), -1, "the panic is the failure value");
        assert_eq!(cb.call((1,)), -1, "and later calls are refused");
    }
    let r = catch_unwind(AssertUnwindSafe(|| cb.finish()));
    assert!(r.is_err(), "finish resumes the panic");
}

#[test]
fn an_owned_callback_is_kept_by_c_and_released() {
    // A Rust closure, surrendered to a C library that keeps it.
    let total = std::sync::Arc::new(AtomicI32::new(0));
    let seen = total.clone();
    let cb: CCallbackBox<Each, _, 1> = CCallbackBox::new(move |x: c_int| {
        seen.fetch_add(x, Ordering::Relaxed);
        0
    });
    // SAFETY: the closure takes any `int`.
    unsafe { cb.call::<_, c_int>((1,)) };
    let (f, ctx, release) = cb.into_c();
    // SAFETY: C's side: calls with the context, then releases it once.
    unsafe {
        f(2, ctx);
        f(3, ctx);
        release.unwrap()(ctx);
    }
    assert_eq!(total.load(Ordering::Relaxed), 6);
    assert_eq!(
        std::sync::Arc::strong_count(&total),
        1,
        "the closure was dropped"
    );

    // A C function with its release, held by Rust and dropped.
    let before = RELEASES.load(Ordering::SeqCst);
    // SAFETY: as in `an_owned_c_callback_releases_its_context_once`.
    let f = unsafe { CFnBox::new(c_add as Each, c_total(), Some(c_total_free)) };
    let cb = CCallbackBox::<Each, fn(c_int) -> c_int, 1>::from_c(f);
    let (g, ctx) = cb.c_form();
    // SAFETY: borrowed while `cb` lives.
    unsafe { g(4, ctx) };
    // SAFETY: the context is live while `cb` is.
    assert_eq!(unsafe { *ctx.cast::<c_int>() }, 4);
    drop(cb);
    assert_eq!(RELEASES.load(Ordering::SeqCst), before + 1);
}

#[test]
fn a_cfn_is_a_copyable_pair() {
    let n = Cell::new(0);
    unsafe extern "C" fn bump(ctx: *mut c_void) -> c_int {
        // SAFETY: the context is a live `Cell<c_int>`.
        let n = unsafe { &*ctx.cast::<Cell<c_int>>() };
        n.set(n.get() + 1);
        n.get()
    }
    type Bump = unsafe extern "C" fn(*mut c_void) -> c_int;
    // SAFETY: `bump` may be called with `&n` while it is borrowed.
    let f: CFn<'_, Bump, 0> = unsafe { CFn::new(bump, core::ptr::from_ref(&n).cast_mut().cast()) };
    let g = f;
    // SAFETY: `bump` takes no other argument.
    unsafe {
        f.call(());
        g.call(());
    }
    assert_eq!(n.get(), 2);
}
