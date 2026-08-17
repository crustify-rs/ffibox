//! Tests that exercise the `define_ctype!`, `impl_dropped!`, `impl_cloned!` and
//! `impl_cvalued!` macros — wiring them to mock C-like state and verifying the
//! trait bindings work end-to-end through `CBox`.
//!
//! `Foo` is the refcounted case (`FOO_free` is a down-ref, cloned via
//! `FOO_up_ref`) and `Bar` the sole-owner case; both ride the same `CBox`.

// Test-only: mock C lifecycle functions deliberately use C-style names,
// and the macro-generated structs aren't worth documenting in a test.
#![allow(non_snake_case, non_camel_case_types, missing_docs)]

use core::cell::Cell;
use core::sync::atomic::{AtomicUsize, Ordering};

use ffibox::{define_ctype, impl_cloned, impl_cvalued, impl_dropped, CBox, CVal};

// ---------------------------------------------------------------------------
// Mock C struct + lifecycle functions
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct foo_st {
    rc: Cell<usize>,
}

#[repr(C)]
pub struct bar_st {
    payload: u64,
}

static FOO_UP_REF_CALLS: AtomicUsize = AtomicUsize::new(0);
static FOO_FREE_CALLS: AtomicUsize = AtomicUsize::new(0);
static FOO_FREED: AtomicUsize = AtomicUsize::new(0);
static BAR_FREE_CALLS: AtomicUsize = AtomicUsize::new(0);
static BAR_DUP_CALLS: AtomicUsize = AtomicUsize::new(0);

/// Payload value that makes the mock `BAR_dup` report failure.
const DUP_FAILS_SENTINEL: u64 = u64::MAX;

/// Mock `FOO_up_ref(p)` — increments refcount, returns 1 on success
/// (matches the OpenSSL convention).
///
/// # Safety
///
/// `p` must point to a live `foo_st`.
unsafe fn FOO_up_ref(p: *mut foo_st) -> i32 {
    FOO_UP_REF_CALLS.fetch_add(1, Ordering::SeqCst);
    // SAFETY: caller guarantees `p` is live.
    let this = unsafe { &*p };
    this.rc.set(this.rc.get() + 1);
    1
}

/// Mock `FOO_free(p)` — decrements refcount, frees on zero.
///
/// # Safety
///
/// `p` must point to a live `foo_st`.
unsafe fn FOO_free(p: *mut foo_st) {
    FOO_FREE_CALLS.fetch_add(1, Ordering::SeqCst);
    // SAFETY: caller guarantees `p` is live.
    let this = unsafe { &*p };
    let new = this.rc.get() - 1;
    this.rc.set(new);
    if new == 0 {
        FOO_FREED.fetch_add(1, Ordering::SeqCst);
        // SAFETY: refcount reached zero; reclaim the Box.
        drop(unsafe { Box::from_raw(p) });
    }
}

/// Mock `BAR_free(p)`.
///
/// # Safety
///
/// `p` must point to a live `bar_st`.
unsafe fn BAR_free(p: *mut bar_st) {
    BAR_FREE_CALLS.fetch_add(1, Ordering::SeqCst);
    // SAFETY: caller guarantees `p` is live.
    drop(unsafe { Box::from_raw(p) });
}

/// Mock `BAR_dup(p)` — deep copy into a fresh allocation, NULL on "failure"
/// (simulated by a payload sentinel, to exercise the `None` path).
///
/// # Safety
///
/// `p` must point to a live `bar_st`.
unsafe fn BAR_dup(p: *mut bar_st) -> *mut bar_st {
    BAR_DUP_CALLS.fetch_add(1, Ordering::SeqCst);
    // SAFETY: caller guarantees `p` is live.
    let payload = unsafe { (*p).payload };
    if payload == DUP_FAILS_SENTINEL {
        return core::ptr::null_mut();
    }
    Box::into_raw(Box::new(bar_st { payload }))
}

// ---------------------------------------------------------------------------
// Generate wrappers via the macros
// ---------------------------------------------------------------------------

define_ctype!(Foo, FooRef, FooMut, foo_st);
// SAFETY: `FOO_up_ref`/`FOO_free` form a correct refcount pair for `foo_st` —
// the down-ref registers as the destructor, the up_ref as the clone.
impl_dropped!(Foo, foo_st, FOO_free);
impl_cloned!(Foo, foo_st, up_ref = FOO_up_ref);

define_ctype!(Bar, BarRef, BarMut, bar_st);
// SAFETY: `BAR_free` is the correct destructor for `bar_st`, and `BAR_dup`
// deep-copies into a fresh allocation releasable by it, NULL on failure.
impl_dropped!(Bar, bar_st, BAR_free);
impl_cloned!(Bar, bar_st, dup = BAR_dup);

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

fn make_foo() -> CBox<Foo> {
    // `Foo` is `#[repr(transparent)]` over `foo_st`, so the layouts match
    // and the cast is sound. We allocate via Rust's `Box`; the lifecycle
    // functions reclaim via `Box::from_raw`.
    let leaked = Box::into_raw(Box::new(foo_st { rc: Cell::new(1) }));
    // SAFETY: leaked pointer is non-null and represents one refcount.
    unsafe { CBox::from_raw(leaked) }.unwrap()
}

fn make_bar(payload: u64) -> CBox<Bar> {
    let leaked = Box::into_raw(Box::new(bar_st { payload }));
    // SAFETY: leaked pointer is non-null and uniquely owned.
    unsafe { CBox::from_raw(leaked) }.unwrap()
}

#[test]
fn macros_refcounted_cbox_lifecycle() {
    FOO_UP_REF_CALLS.store(0, Ordering::SeqCst);
    FOO_FREE_CALLS.store(0, Ordering::SeqCst);
    FOO_FREED.store(0, Ordering::SeqCst);

    let a = make_foo();
    // SAFETY: live `foo_st`; read the refcount field through the raw pointer
    // (`CBox::as_ptr` yields `*mut Foo`, layout-identical to `*mut foo_st`).
    assert_eq!(unsafe { (*a.as_ptr().cast::<foo_st>()).rc.get() }, 1);

    let b = a.clone();
    assert_eq!(FOO_UP_REF_CALLS.load(Ordering::SeqCst), 1);
    // SAFETY: as above.
    assert_eq!(unsafe { (*b.as_ptr().cast::<foo_st>()).rc.get() }, 2);

    drop(a);
    assert_eq!(FOO_FREE_CALLS.load(Ordering::SeqCst), 1);
    assert_eq!(FOO_FREED.load(Ordering::SeqCst), 0);

    drop(b);
    assert_eq!(FOO_FREE_CALLS.load(Ordering::SeqCst), 2);
    assert_eq!(FOO_FREED.load(Ordering::SeqCst), 1);
}

#[test]
fn macros_cbox_lifecycle() {
    BAR_FREE_CALLS.store(0, Ordering::SeqCst);

    let b = make_bar(0xdead_beef);
    // SAFETY: live `bar_st`; read the field through the raw pointer
    // (`CBox::as_ptr` yields `*mut Bar`, layout-identical to `*mut bar_st`).
    assert_eq!(
        unsafe { (*b.as_ptr().cast::<bar_st>()).payload },
        0xdead_beef
    );
    drop(b);
    assert_eq!(BAR_FREE_CALLS.load(Ordering::SeqCst), 1);
}

#[test]
fn macros_dup_clone_deep_copies() {
    BAR_FREE_CALLS.store(0, Ordering::SeqCst);
    BAR_DUP_CALLS.store(0, Ordering::SeqCst);

    let a = make_bar(0x1234);
    let b = a.clone();
    assert_eq!(BAR_DUP_CALLS.load(Ordering::SeqCst), 1);

    // A deep copy is a *distinct* allocation, unlike the up_ref case.
    assert_ne!(a.as_ptr(), b.as_ptr());
    // SAFETY: both are live `bar_st`; read the payload through the raw pointer.
    assert_eq!(unsafe { (*b.as_ptr().cast::<bar_st>()).payload }, 0x1234);

    drop(a);
    drop(b);
    assert_eq!(BAR_FREE_CALLS.load(Ordering::SeqCst), 2);
}

#[test]
fn macros_dup_try_clone_reports_failure_as_none() {
    BAR_FREE_CALLS.store(0, Ordering::SeqCst);

    let a = make_bar(DUP_FAILS_SENTINEL);
    // The mock returns NULL, which the macro must surface as `None` rather
    // than fabricating a handle.
    assert!(a.try_clone().is_none());

    drop(a);
    assert_eq!(BAR_FREE_CALLS.load(Ordering::SeqCst), 1);
}

#[test]
fn the_shared_handle_reads_and_the_exclusive_one_writes() {
    let raw = Box::into_raw(Box::new(bar_st { payload: 7 }));

    // SAFETY: `raw` is non-null and addresses a valid `bar_st`.
    let r: BarRef<'_> = unsafe { BarRef::from_ptr(raw) }.unwrap();
    // SAFETY: read through the raw pointer; no reference to `Bar` is formed.
    assert_eq!(unsafe { (*r.as_ptr()).payload }, 7);
    assert_eq!(r.as_ptr(), raw.cast_const());

    // The shared handle has no write path: `as_ptr` is `*const`. Writing needs
    // the exclusive handle.
    // SAFETY: sole handle to the object for the rest of this scope.
    let mut m: BarMut<'_> = unsafe { BarMut::from_ptr(raw) }.unwrap();
    // SAFETY: `&mut self` carries write provenance.
    unsafe { (*m.as_mut_ptr()).payload = 99 };
    // Getters reach through `Deref` to the shared handle.
    // SAFETY: written on the line above.
    assert_eq!(unsafe { (*m.as_ptr()).payload }, 99);

    // The handles are one pointer, never the object.
    assert_eq!(
        core::mem::size_of::<BarRef<'_>>(),
        core::mem::size_of::<*const bar_st>()
    );
    assert_eq!(
        core::mem::size_of::<BarMut<'_>>(),
        core::mem::size_of::<*const bar_st>()
    );

    // SAFETY: only one outstanding pointer (`raw`); no aliasing.
    drop(unsafe { Box::from_raw(raw) });
}

#[test]
fn define_ctype_from_ptr_null_returns_none() {
    // SAFETY: passing null is the documented `None` case.
    let r: Option<BarRef<'_>> = unsafe { BarRef::from_ptr(core::ptr::null_mut()) };
    assert!(r.is_none());
    // SAFETY: as above, on the exclusive handle.
    let m: Option<BarMut<'_>> = unsafe { BarMut::from_ptr(core::ptr::null_mut()) };
    assert!(m.is_none());
}

#[test]
fn define_type_void_ptr_seam_round_trips() {
    let raw = Box::into_raw(Box::new(bar_st { payload: 42 }));
    // SAFETY: `raw` is non-null and points to a valid `bar_st`.
    let r: BarRef<'_> = unsafe { BarRef::from_ptr(raw) }.unwrap();

    // Erase to `void*` and reconstitute — the `as_void_ptr` / `from_void_ptr`
    // pair, standing in for a C slot that stores an opaque pointer.
    let erased = r.as_void_ptr().cast_mut();
    // SAFETY: `erased` was erased from this very `Bar`, which is still live.
    let back: BarRef<'_> = unsafe { BarRef::from_void_ptr(erased) }.unwrap();

    assert_eq!(back.as_ptr(), raw.cast_const());
    // SAFETY: read the field through the reconstituted borrow.
    assert_eq!(unsafe { (*back.as_ptr()).payload }, 42);

    // Reclaim the leak.
    // SAFETY: only one outstanding pointer (`raw`); no aliasing.
    drop(unsafe { Box::from_raw(raw) });
}

#[test]
fn define_type_from_void_ptr_null_returns_none() {
    // SAFETY: passing null is the documented `None` case.
    let r: Option<BarRef<'_>> = unsafe { BarRef::from_void_ptr(core::ptr::null_mut()) };
    assert!(r.is_none());
}

#[test]
fn define_ctype_is_repr_transparent() {
    // `Foo` is #[repr(transparent)] over `CType<foo_st>` (= foo_st + a ZST), so
    // it keeps the C layout and embeds by value in a `#[repr(C)]` mirror.
    assert_eq!(core::mem::size_of::<Foo>(), core::mem::size_of::<foo_st>());
    assert_eq!(core::mem::size_of::<Bar>(), core::mem::size_of::<bar_st>());
}

// ---------------------------------------------------------------------------
// impl_cvalued! — by-value owned-resource macro, exercised via CVal
// ---------------------------------------------------------------------------

static QUX_DISPOSE_CALLS: AtomicUsize = AtomicUsize::new(0);

#[repr(C)]
#[derive(Default)]
pub struct qux_st {
    payload: u64,
}

/// Mock `qux_dispose(p)` — disposes the owned resource WITHOUT freeing the
/// by-value header (Rust owns it inline; nothing to `Box::from_raw` here).
///
/// # Safety
///
/// `p` must point to a live, initialised `qux_st`.
unsafe fn QUX_dispose(_p: *mut qux_st) {
    QUX_DISPOSE_CALLS.fetch_add(1, Ordering::SeqCst);
}

define_ctype!(Qux, QuxRef, QuxMut, qux_st);
// SAFETY: `QUX_dispose` disposes `qux_st`'s owned resource without freeing
// the header; `Qux` is layout-compatible with `*mut qux_st`.
impl_cvalued!(Qux, qux_st, QUX_dispose);

#[test]
fn macros_cvalue_disposes_once_on_drop() {
    QUX_DISPOSE_CALLS.store(0, Ordering::SeqCst);

    // Base `zeroed()` (from define_type!) + CVal: the whole point — no
    // hand-written `<T>Stack` companion, no hand-written `impl Drop`.
    let v = CVal::new(Qux::zeroed());
    // SAFETY: zeroed `qux_st` is a valid state; read through the shared handle
    // `CVal::as_ref` hands out -- no reference to `Qux` is formed.
    assert_eq!(unsafe { (*v.as_ref().as_ptr()).payload }, 0);
    drop(v);
    assert_eq!(QUX_DISPOSE_CALLS.load(Ordering::SeqCst), 1);
}

#[test]
fn inline_storage_gates_read_against_write_the_ordinary_way() {
    // `CVal` owns the value inline, so `&self` / `&mut self` do the gating and
    // the pointer comes from `addr_of!` / `addr_of_mut!` -- no `&Qux` anywhere.
    let mut v = CVal::new(Qux::zeroed());
    // SAFETY: zeroed `qux_st` is valid; write through the exclusive handle.
    unsafe { (*v.as_mut().as_mut_ptr()).payload = 5 };
    // SAFETY: written on the line above.
    assert_eq!(unsafe { (*v.as_ref().as_ptr()).payload }, 5);
}
