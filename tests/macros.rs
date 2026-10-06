//! Tests that exercise `define_ctype!` and the policy macros (`impl_cdrop!`,
//! `impl_cdupclone!`, `impl_crefclone!`, `impl_clendrop!`, `impl_clenclone!`,
//! `impl_cdispose!`, and the `_str` variants), wiring them to mock
//! C-like state and driving them through the owners: `CBox`,
//! `CStrBox`, `CVec`, `CVal` and the run views.
//!
//! `FooOwned` is a refcounted object's sole reference (`FOO_free` is a
//! down-ref, `FOO_up_ref` its `CRefClone`) and `BarOwned` a sole owner with a
//! deep copy. The owners are exercised over macro-bound and hand-written
//! policies, typed and `void`, with and without a clone, `Default` and
//! stateful.
//!
//! The policy macros call the routine with a pointer of the exact C type, so
//! binding a routine for the wrong type is rejected at compile time rather
//! than cast silently; a `compile_fail` doctest on `impl_cdrop!` checks it.

// Test-only: mock C lifecycle functions deliberately use C-style names,
// and the macro-generated structs aren't worth documenting in a test.
#![allow(non_snake_case, non_camel_case_types, missing_docs)]

use core::cell::Cell;
use core::sync::atomic::{AtomicUsize, Ordering};

use core::ffi::{c_char, CStr};
use core::ptr::NonNull;
use std::ffi::CString;

use ffibox::{
    define_ctype, impl_cdispose, impl_cdrop, impl_cdrop_str, impl_cdupclone, impl_cdupclone_str,
    impl_clenclone, impl_clendrop, impl_crefclone, CBox, CDrop, CDupClone, CLenDrop, CRefClone,
    CSlice, CSliceMut, CStrBox, CVal, CVec,
};

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

/// Serialises the tests that count through the `FOO_*` / `BAR_*` statics, and
/// every test that frees a `Bar` through them.
static FOO_BAR_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn foo_bar_lock() -> std::sync::MutexGuard<'static, ()> {
    FOO_BAR_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

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
// `FOO_up_ref`/`FOO_free` form a correct refcount pair for `foo_st` — the
// down-ref registers as the destructor, the up_ref as its `CRefClone`.
#[derive(Clone, Copy, Debug, Default)]
pub struct FooUnref;
impl_cdrop!(FooUnref, Foo, FOO_free);
impl_crefclone!(FooUnref, Foo, FOO_up_ref, ok = |r| r == 1);
pub type FooOwned = CBox<Foo, FooUnref>;

define_ctype!(Bar, BarRef, BarMut, bar_st);
// `BAR_free` is the correct destructor for `bar_st`, and `BAR_dup` deep-copies
// into a fresh allocation releasable by it, NULL on failure.
#[derive(Clone, Copy, Debug, Default)]
pub struct BarFree;
impl_cdrop!(BarFree, Bar, BAR_free);
impl_cdupclone!(BarFree, Bar, BAR_dup);
pub type BarOwned = CBox<Bar, BarFree>;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

fn make_foo() -> FooOwned {
    // `Foo` is `#[repr(transparent)]` over `foo_st`, so the layouts match
    // and the cast is sound. We allocate via Rust's `Box`; the lifecycle
    // functions reclaim via `Box::from_raw`.
    let leaked = Box::into_raw(Box::new(foo_st { rc: Cell::new(1) }));
    // SAFETY: leaked pointer is non-null and represents one refcount.
    unsafe { FooOwned::from_c(leaked) }.unwrap()
}

fn make_bar(payload: u64) -> BarOwned {
    let leaked = Box::into_raw(Box::new(bar_st { payload }));
    // SAFETY: leaked pointer is non-null and uniquely owned.
    unsafe { BarOwned::from_c(leaked) }.unwrap()
}

#[test]
fn macros_refcounted_owned_lifecycle() {
    let _serial = foo_bar_lock();
    FOO_UP_REF_CALLS.store(0, Ordering::SeqCst);
    FOO_FREE_CALLS.store(0, Ordering::SeqCst);
    FOO_FREED.store(0, Ordering::SeqCst);

    let a = make_foo();
    // SAFETY: live `foo_st`; read the refcount field through the raw pointer,
    // which `as_c_ptr` types as `*mut foo_st`.
    assert_eq!(unsafe { (*a.as_c_ptr()).rc.get() }, 1);

    // The macro-bound up_ref, as a shared owner would call it.
    let p = NonNull::new(a.as_ptr()).unwrap();
    // SAFETY: `a` keeps the object live; the bump is settled below.
    assert!(unsafe { a.policy().c_up_ref(p) });
    assert_eq!(FOO_UP_REF_CALLS.load(Ordering::SeqCst), 1);
    // SAFETY: as above.
    assert_eq!(unsafe { (*a.as_c_ptr()).rc.get() }, 2);

    // SAFETY: settles the reference taken above.
    unsafe { a.policy().c_drop(p) };
    assert_eq!(FOO_FREE_CALLS.load(Ordering::SeqCst), 1);
    assert_eq!(FOO_FREED.load(Ordering::SeqCst), 0);

    drop(a);
    assert_eq!(FOO_FREE_CALLS.load(Ordering::SeqCst), 2);
    assert_eq!(FOO_FREED.load(Ordering::SeqCst), 1);
}

#[test]
fn macros_owned_lifecycle() {
    let _serial = foo_bar_lock();
    BAR_FREE_CALLS.store(0, Ordering::SeqCst);

    let b = make_bar(0xdead_beef);
    // SAFETY: live `bar_st`; read the field through the shared handle.
    assert_eq!(unsafe { (*b.as_ref().as_ptr()).payload }, 0xdead_beef);
    drop(b);
    assert_eq!(BAR_FREE_CALLS.load(Ordering::SeqCst), 1);
}

#[test]
fn macros_dup_clone_deep_copies() {
    let _serial = foo_bar_lock();
    BAR_FREE_CALLS.store(0, Ordering::SeqCst);
    BAR_DUP_CALLS.store(0, Ordering::SeqCst);

    let a = make_bar(0x1234);
    let b = a.clone();
    assert_eq!(BAR_DUP_CALLS.load(Ordering::SeqCst), 1);

    // A deep copy is a *distinct* allocation, unlike an up_ref.
    assert_ne!(a.as_ptr(), b.as_ptr());
    // SAFETY: both are live `bar_st`; read the payload through the raw pointer.
    assert_eq!(unsafe { (*b.as_c_ptr()).payload }, 0x1234);

    drop(a);
    drop(b);
    assert_eq!(BAR_FREE_CALLS.load(Ordering::SeqCst), 2);
}

#[test]
fn macros_dup_try_clone_reports_failure_as_none() {
    let _serial = foo_bar_lock();
    BAR_FREE_CALLS.store(0, Ordering::SeqCst);

    let a = make_bar(DUP_FAILS_SENTINEL);
    // The mock returns NULL, which must surface as `None` rather
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
    // Reading through the exclusive handle goes via `as_ref`, which binds the
    // shared handle to this borrow. There is no `Deref`: its target would carry
    // the handle's own lifetime, and a `Copy` shared handle would then escape
    // the reborrow.
    // SAFETY: written on the line above.
    assert_eq!(unsafe { (*m.as_ref().as_ptr()).payload }, 99);

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
    // `Foo` is #[repr(transparent)] over `foo_st` (plus a ZST marker), so
    // it keeps the C layout and embeds by value in a `#[repr(C)]` mirror.
    assert_eq!(core::mem::size_of::<Foo>(), core::mem::size_of::<foo_st>());
    assert_eq!(core::mem::size_of::<Bar>(), core::mem::size_of::<bar_st>());
}

// ---------------------------------------------------------------------------
// CVal — a value held inline, disposed on drop
// ---------------------------------------------------------------------------

static QUX_DISPOSE_CALLS: AtomicUsize = AtomicUsize::new(0);
static GATED_DISPOSE_CALLS: AtomicUsize = AtomicUsize::new(0);

#[repr(C)]
#[derive(Default)]
pub struct qux_st {
    payload: u64,
    /// Disposal gate: the disposer always runs; whether it reclaims anything
    /// folds into the routine itself (the recommended pattern).
    owns: u8,
}

/// Mock `qux_dispose(p)` — releases the owned resource WITHOUT freeing the
/// struct, which is Rust's inline storage.
///
/// # Safety
///
/// `p` must point to a live, initialised `qux_st`.
unsafe fn QUX_dispose(p: *mut qux_st) {
    QUX_DISPOSE_CALLS.fetch_add(1, Ordering::SeqCst);
    // SAFETY: the caller guarantees `p` is live.
    if unsafe { (*p).owns } != 0 {
        GATED_DISPOSE_CALLS.fetch_add(1, Ordering::SeqCst);
    }
}

define_ctype!(Qux, QuxRef, QuxMut, qux_st);
#[derive(Clone, Copy, Debug, Default)]
pub struct QuxDispose;
impl_cdispose!(QuxDispose, Qux, QUX_dispose);
/// A `qux_st` held inline and disposed on drop.
pub type QuxVal = CVal<Qux, QuxDispose>;

/// Serialises the tests sharing the counters above.
static QUX_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn cval_disposes_once_on_drop() {
    let _serial = QUX_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    QUX_DISPOSE_CALLS.store(0, Ordering::SeqCst);

    let v = QuxVal::new(Qux::zeroed());
    // SAFETY: zeroed `qux_st` is valid; read through the shared handle.
    assert_eq!(unsafe { (*v.as_ref().as_ptr()).payload }, 0);
    drop(v);
    assert_eq!(QUX_DISPOSE_CALLS.load(Ordering::SeqCst), 1);
}

#[test]
fn cval_gates_read_against_write_the_ordinary_way() {
    // The value is Rust's inline storage, so `&self` / `&mut self` do the
    // gating and the handles come from them.
    let mut v = QuxVal::new(Qux::zeroed());
    // SAFETY: zeroed `qux_st` is valid; write through the exclusive handle.
    unsafe { (*v.as_mut().as_mut_ptr()).payload = 5 };
    // SAFETY: written on the line above.
    assert_eq!(unsafe { (*v.as_ref().as_ptr()).payload }, 5);
}

#[test]
fn cval_disposer_gate_folds_internally() {
    let _serial = QUX_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    GATED_DISPOSE_CALLS.store(0, Ordering::SeqCst);

    drop(QuxVal::new(Qux::zeroed())); // owns == 0: the disposer runs, reclaims nothing
    assert_eq!(GATED_DISPOSE_CALLS.load(Ordering::SeqCst), 0);

    let mut v = QuxVal::new(Qux::zeroed());
    // SAFETY: write through the exclusive handle.
    unsafe { (*v.as_mut().as_mut_ptr()).owns = 1 };
    drop(v);
    assert_eq!(GATED_DISPOSE_CALLS.load(Ordering::SeqCst), 1);
}

#[test]
fn cval_into_inner_gives_up_disposal() {
    let _serial = QUX_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    QUX_DISPOSE_CALLS.store(0, Ordering::SeqCst);

    // A bare `Qux` has no `Drop`: going out of scope disposes nothing.
    let _bare: Qux = QuxVal::new(Qux::zeroed()).into_inner();
    assert_eq!(QUX_DISPOSE_CALLS.load(Ordering::SeqCst), 0);
    assert_eq!(
        core::mem::size_of::<QuxVal>(),
        core::mem::size_of::<qux_st>()
    );
}

// ---------------------------------------------------------------------------
// CBox — common surface: raw seam round trip, layout
// ---------------------------------------------------------------------------

// Each test below counts through its own static: tests run in parallel.
static SEAM_FREES: AtomicUsize = AtomicUsize::new(0);

/// # Safety
///
/// `p` must be a live, uniquely-owned `bar_st` from `Box::into_raw`.
unsafe fn bar_seam_free(p: *mut bar_st) {
    SEAM_FREES.fetch_add(1, Ordering::SeqCst);
    // SAFETY: caller transfers a `Box`-allocated `bar_st`.
    drop(unsafe { Box::from_raw(p) });
}

#[derive(Clone, Copy, Debug, Default)]
pub struct BarSeamFree;
impl_cdrop!(BarSeamFree, Bar, bar_seam_free);
pub type BarSeamOwned = CBox<Bar, BarSeamFree>;

#[test]
fn owned_raw_seam_round_trips_without_teardown() {
    let leaked = Box::into_raw(Box::new(bar_st { payload: 3 }));
    // SAFETY: leaked pointer is non-null and uniquely owned.
    let a = unsafe { BarSeamOwned::from_c(leaked) }.unwrap();
    let raw = a.into_c();
    assert_eq!(raw, leaked);
    assert_eq!(SEAM_FREES.load(Ordering::SeqCst), 0, "into_c defuses");
    // SAFETY: `raw` came from `into_c` and still owes its one free.
    let a = unsafe { BarSeamOwned::from_c(raw) }.unwrap();
    assert_eq!(a.as_c_ptr(), raw);
    assert_eq!(a.as_ptr().cast::<bar_st>(), raw);
    // The generic seam speaks the layout type and round-trips the same way.
    let raw = a.into_raw();
    // SAFETY: `raw` came from `into_raw` and still owes its one free.
    let a = unsafe { BarSeamOwned::from_raw(raw) }.unwrap();
    // SAFETY: null is the documented `None` case.
    assert!(unsafe { BarSeamOwned::from_c(core::ptr::null_mut()) }.is_none());
    drop(a);
    assert_eq!(SEAM_FREES.load(Ordering::SeqCst), 1);
}

#[test]
fn owned_box_is_a_niche_pointer() {
    use core::mem::size_of;
    assert_eq!(size_of::<FooOwned>(), size_of::<*mut foo_st>());
    assert_eq!(size_of::<Option<BarOwned>>(), size_of::<*mut bar_st>());
    // The macro-bound policies are ZSTs.
    assert_eq!(size_of::<FooUnref>(), 0);
    assert_eq!(size_of::<BarFree>(), 0);
}

#[test]
fn owned_as_mut_writes_through_the_exclusive_handle() {
    let _serial = foo_bar_lock();
    let mut b = make_bar(1);
    // SAFETY: the exclusive handle carries write provenance.
    unsafe { (*b.as_mut().as_mut_ptr()).payload = 2 };
    // SAFETY: written on the line above.
    assert_eq!(unsafe { (*b.as_ref().as_ptr()).payload }, 2);
}

// ---------------------------------------------------------------------------
// impl_cdrop! — a second destructor for the same type, via an adapter fn
// ---------------------------------------------------------------------------

static BAR_SLOT_FREES: AtomicUsize = AtomicUsize::new(0);

/// Mock `BAR_free_slot(&p)` — the `av_*_free(T **)` shape: frees `*pp` and
/// nulls the slot.
///
/// # Safety
///
/// `pp` must point to a slot holding a live `bar_st` pointer.
unsafe fn BAR_free_slot(pp: *mut *mut bar_st) {
    BAR_SLOT_FREES.fetch_add(1, Ordering::SeqCst);
    // SAFETY: caller guarantees the slot holds a live, owned pointer.
    unsafe {
        drop(Box::from_raw(*pp));
        *pp = core::ptr::null_mut();
    }
}

/// Adapter giving the slot-pointer destructor the `*mut bar_st` shape the
/// policy macros call.
///
/// # Safety
///
/// `p` must be a live, uniquely-owned `bar_st`.
unsafe fn bar_free_via_slot(mut p: *mut bar_st) {
    // SAFETY: caller transfers `p`; the local slot is writable.
    unsafe { BAR_free_slot(&mut p) }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct BarSlotFree;
impl_cdrop!(BarSlotFree, Bar, bar_free_via_slot);
/// The same `Bar`, released through the slot-pointer destructor: a second
/// box type, so the type says which teardown runs.
pub type BarSlotOwned = CBox<Bar, BarSlotFree>;

#[test]
fn slot_pointer_destructor_form() {
    let _serial = foo_bar_lock();
    let leaked = Box::into_raw(Box::new(bar_st { payload: 5 }));
    // SAFETY: leaked pointer is non-null and uniquely owned.
    let b = unsafe { BarSlotOwned::from_c(leaked) }.unwrap();
    drop(b);
    assert_eq!(BAR_SLOT_FREES.load(Ordering::SeqCst), 1);
}

/// Compiles only when `T` is NOT `Clone` (both impls would apply to a `Clone`
/// type, making `_` ambiguous).
trait AmbiguousIfClone<A> {
    fn check() {}
}
impl<T> AmbiguousIfClone<()> for T {}
impl<T: Clone> AmbiguousIfClone<u8> for T {}

#[test]
fn a_policy_without_cdupclone_yields_a_non_clone_owner() {
    // `CBox: Clone` needs `Policy: CDupClone<T> + Clone`; with no `CDupClone`
    // on the policy the impl never applies, an up_ref included.
    <BarSeamOwned as AmbiguousIfClone<_>>::check();
    <BarSlotOwned as AmbiguousIfClone<_>>::check();
    <FooOwned as AmbiguousIfClone<_>>::check();
    // The same holds for a stateful policy; one with `CDupClone + Clone` clones.
    <BarTagged as AmbiguousIfClone<_>>::check();
    fn is_clone<T: Clone>() {}
    is_clone::<BarTaggedDup>();
    is_clone::<BarOwned>();
}

// ---------------------------------------------------------------------------
// CBox — hand-written policies (typed and void, with/without clone)
// ---------------------------------------------------------------------------

static TAGGED_FREES: AtomicUsize = AtomicUsize::new(0);
static TAGGED_DUP_FREES: AtomicUsize = AtomicUsize::new(0);
static TAGGED_DUPS: AtomicUsize = AtomicUsize::new(0);

/// A stateful policy: carries the tag it stamps on teardown. Deliberately not
/// `Default` (nor `Clone`), so its box is built with `from_c_with` /
/// `from_raw_with`: `from_raw` needs `Tagged: Default`.
pub struct Tagged(u64);

// SAFETY: frees the Box-backed mock `bar_st` exactly once.
unsafe impl CDrop<Bar> for Tagged {
    unsafe fn c_drop(&self, ptr: NonNull<Bar>) {
        TAGGED_FREES.fetch_add(self.0 as usize, Ordering::SeqCst);
        // SAFETY: caller upholds the trait contract.
        drop(unsafe { Box::from_raw(ptr.as_ptr().cast::<bar_st>()) });
    }
}

/// A stateful policy that can also clone.
#[derive(Clone)]
pub struct TaggedDup(u64);

// SAFETY: as `Tagged`.
unsafe impl CDrop<Bar> for TaggedDup {
    unsafe fn c_drop(&self, ptr: NonNull<Bar>) {
        TAGGED_DUP_FREES.fetch_add(self.0 as usize, Ordering::SeqCst);
        // SAFETY: caller upholds the trait contract.
        drop(unsafe { Box::from_raw(ptr.as_ptr().cast::<bar_st>()) });
    }
}

// SAFETY: deep-copies into a fresh Box this policy frees.
unsafe impl CDupClone<Bar> for TaggedDup {
    unsafe fn c_dup(&self, ptr: NonNull<Bar>) -> Option<NonNull<Bar>> {
        TAGGED_DUPS.fetch_add(1, Ordering::SeqCst);
        // SAFETY: caller guarantees `ptr` is live.
        let payload = unsafe { (*ptr.as_ptr().cast::<bar_st>()).payload };
        NonNull::new(Box::into_raw(Box::new(bar_st { payload })).cast())
    }
}

pub type BarTagged = CBox<Bar, Tagged>;
pub type BarTaggedDup = CBox<Bar, TaggedDup>;

#[test]
fn stateful_non_default_policy_works_through_from_raw_with() {
    let raw = Box::into_raw(Box::new(bar_st { payload: 1 }));
    // SAFETY: fresh, uniquely-owned allocation that `Tagged` frees.
    let b = unsafe { BarTagged::from_c_with(raw, Tagged(10)) }.unwrap();
    // SAFETY: live `bar_st`.
    assert_eq!(unsafe { (*b.as_ref().as_ptr()).payload }, 1);
    // A stateful policy makes the owner fat.
    assert_eq!(
        core::mem::size_of::<BarTagged>(),
        core::mem::size_of::<*mut bar_st>() + core::mem::size_of::<u64>()
    );

    // `into_raw_with` hands the policy back with the pointer.
    let (raw, Tagged(tag)) = b.into_raw_with();
    assert_eq!(tag, 10);
    assert_eq!(TAGGED_FREES.load(Ordering::SeqCst), 0);
    // SAFETY: re-adopt what `into_raw_with` surrendered.
    drop(unsafe { BarTagged::from_raw_with(raw, Tagged(3)) });
    assert_eq!(TAGGED_FREES.load(Ordering::SeqCst), 3, "the new policy ran");
}

#[test]
fn hand_written_policy_with_clone() {
    let raw = Box::into_raw(Box::new(bar_st { payload: 8 }));
    // SAFETY: fresh, uniquely-owned allocation that `TaggedDup` frees.
    let a = unsafe { BarTaggedDup::from_c_with(raw, TaggedDup(1)) }.unwrap();
    let b = a.try_clone().unwrap();
    let c = b.clone();
    assert_eq!(TAGGED_DUPS.load(Ordering::SeqCst), 2);
    // SAFETY: live `bar_st`.
    assert_eq!(unsafe { (*c.as_c_ptr()).payload }, 8);
    drop((a, b, c));
    assert_eq!(
        TAGGED_DUP_FREES.load(Ordering::SeqCst),
        3,
        "the policy was cloned too"
    );
}

// ---------------------------------------------------------------------------
// CStrBox — macro-bound and hand-written policies
// ---------------------------------------------------------------------------

static STR_FREES: AtomicUsize = AtomicUsize::new(0);
static TAGGED_STR_FREES: AtomicUsize = AtomicUsize::new(0);

/// # Safety
///
/// `p` must come from `CString::into_raw`.
unsafe fn mock_str_free(p: *mut c_char) {
    STR_FREES.fetch_add(1, Ordering::SeqCst);
    // SAFETY: caller guarantees the provenance.
    drop(unsafe { CString::from_raw(p) });
}

/// # Safety
///
/// `p` must be a live NUL-terminated string.
unsafe fn mock_strdup(p: *mut c_char) -> *mut c_char {
    // SAFETY: caller guarantees a live NUL-terminated string.
    let s = unsafe { CStr::from_ptr(p) };
    if s.to_bytes() == b"nodup" {
        return core::ptr::null_mut();
    }
    s.to_owned().into_raw()
}

fn mock_str(s: &str) -> *mut c_char {
    CString::new(s).unwrap().into_raw()
}

#[derive(Clone, Copy, Debug, Default)]
pub struct MockStrFree;
impl_cdrop_str!(MockStrFree, mock_str_free);
impl_cdupclone_str!(MockStrFree, mock_strdup);
/// A string from the mock allocator.
pub type MockStr = CStrBox<MockStrFree>;

/// A hand-written string policy carrying a tag.
#[derive(Clone)]
pub struct StrTagged(usize);

// SAFETY: frees a `CString::into_raw` string exactly once.
unsafe impl CDrop<c_char> for StrTagged {
    unsafe fn c_drop(&self, ptr: NonNull<c_char>) {
        TAGGED_STR_FREES.fetch_add(self.0, Ordering::SeqCst);
        // SAFETY: caller upholds the trait contract.
        drop(unsafe { CString::from_raw(ptr.as_ptr()) });
    }
}

// SAFETY: copies into a fresh `CString` this policy frees.
unsafe impl CDupClone<c_char> for StrTagged {
    unsafe fn c_dup(&self, ptr: NonNull<c_char>) -> Option<NonNull<c_char>> {
        // SAFETY: caller upholds the trait contract.
        NonNull::new(unsafe { mock_strdup(ptr.as_ptr()) })
    }
}

pub type TaggedStr = CStrBox<StrTagged>;

#[test]
fn cstr_macro_bound_policy() {
    // SAFETY: a fresh, uniquely-owned NUL-terminated string.
    let s = unsafe { MockStr::from_raw(mock_str("héllo")) }.unwrap();
    assert_eq!(s.to_str(), Ok("héllo"));
    assert_eq!(s.as_bytes(), "héllo".as_bytes());
    assert_eq!(s.len(), 6);
    assert!(!s.is_empty());
    assert_eq!(s.as_c_str().to_bytes().len(), 6);

    let t = s.clone();
    assert_ne!(s.as_ptr(), t.as_ptr());
    assert_eq!(
        core::mem::size_of::<Option<MockStr>>(),
        core::mem::size_of::<*mut c_char>()
    );

    // SAFETY: as above.
    let n = unsafe { MockStr::from_raw(mock_str("nodup")) }.unwrap();
    assert!(
        n.try_clone().is_none(),
        "NULL from the dup surfaces as None"
    );

    // `into_raw` defuses; re-adopt to free.
    let raw = t.into_raw();
    // SAFETY: from `into_raw` above.
    let t = unsafe { MockStr::from_raw(raw) }.unwrap();
    // SAFETY: null is the documented `None` case.
    assert!(unsafe { MockStr::from_raw(core::ptr::null_mut()) }.is_none());

    drop((s, t, n));
    assert_eq!(STR_FREES.load(Ordering::SeqCst), 3);
}

#[test]
fn cstr_hand_written_policy() {
    // SAFETY: a fresh, uniquely-owned NUL-terminated string.
    let s = unsafe { TaggedStr::from_raw_with(mock_str(""), StrTagged(5)) }.unwrap();
    assert!(s.is_empty());
    let t = s.try_clone().unwrap();
    let (raw, StrTagged(tag)) = t.into_raw_with();
    assert_eq!(tag, 5, "the policy was cloned with the string");
    // SAFETY: from `into_raw_with` above.
    drop(unsafe { TaggedStr::from_raw_with(raw, StrTagged(1)) });
    drop(s);
    assert_eq!(TAGGED_STR_FREES.load(Ordering::SeqCst), 6);
}

// ---------------------------------------------------------------------------
// CVec — macro-bound and hand-written policies
// ---------------------------------------------------------------------------

static VEC_FREES: AtomicUsize = AtomicUsize::new(0);
static TAGGED_VEC_FREES: AtomicUsize = AtomicUsize::new(0);

/// Every mock buffer is allocated with this alignment, which covers each
/// element type used below.
const VEC_ALIGN: usize = 8;

fn vec_layout(byte_len: usize) -> std::alloc::Layout {
    std::alloc::Layout::from_size_align(byte_len.max(1), VEC_ALIGN).unwrap()
}

/// # Safety
///
/// `ptr` must come from `mock_vec_alloc(byte_len)`.
unsafe fn mock_vec_free(ptr: *mut u8, byte_len: usize) {
    VEC_FREES.fetch_add(1, Ordering::SeqCst);
    // SAFETY: caller guarantees the allocation's layout.
    unsafe { std::alloc::dealloc(ptr, vec_layout(byte_len)) }
}

fn mock_vec_alloc(byte_len: usize) -> *mut u8 {
    // SAFETY: the layout is never zero-sized.
    unsafe { std::alloc::alloc(vec_layout(byte_len)) }
}

/// # Safety
///
/// `ptr` must hold `byte_len` live bytes.
unsafe fn mock_memdup(ptr: *mut u8, byte_len: usize) -> *mut u8 {
    let copy = mock_vec_alloc(byte_len);
    // SAFETY: both hold `byte_len` bytes and do not overlap.
    unsafe { core::ptr::copy_nonoverlapping(ptr, copy, byte_len) };
    copy
}

fn mock_array<T: Copy>(items: &[T]) -> *mut T {
    let p = mock_vec_alloc(core::mem::size_of_val(items)).cast::<T>();
    // SAFETY: `p` holds `items.len()` elements and does not overlap `items`.
    unsafe { core::ptr::copy_nonoverlapping(items.as_ptr(), p, items.len()) };
    p
}

#[derive(Clone, Copy, Debug, Default)]
pub struct MockVecFree;
impl_clendrop!(MockVecFree, mock_vec_free);
impl_clenclone!(MockVecFree, mock_memdup);
/// A buffer from the mock allocator.
pub type MockVec<T> = CVec<T, MockVecFree>;

/// A hand-written buffer policy carrying a tag.
pub struct VecTagged(usize);

// SAFETY: frees a `mock_vec_alloc` buffer exactly once.
unsafe impl CLenDrop for VecTagged {
    unsafe fn c_drop_len(&self, ptr: *mut u8, byte_len: usize) {
        TAGGED_VEC_FREES.fetch_add(self.0, Ordering::SeqCst);
        // SAFETY: caller upholds the trait contract.
        unsafe { std::alloc::dealloc(ptr, vec_layout(byte_len)) }
    }
}

pub type TaggedVec<T> = CVec<T, VecTagged>;

#[test]
fn cvec_macro_bound_policy() {
    // SAFETY: 3 initialised `u32`s from the mock allocator.
    let mut v = unsafe { MockVec::from_raw_parts(mock_array(&[1u32, 2, 3]), 3) }.unwrap();
    assert_eq!(v.len(), 3);
    assert_eq!(v.byte_len(), 12);
    assert!(!v.is_empty());
    assert_eq!(v.as_slice(), &[1, 2, 3]);

    let w = v.clone();
    v.as_mut_slice()[0] = 9;
    assert_eq!(w.as_slice(), &[1, 2, 3], "the clone is independent");
    assert_ne!(v.as_ptr(), w.as_ptr());

    let (ptr, count) = w.into_raw_parts();
    assert_eq!(count, 3);
    // SAFETY: re-adopt what `into_raw_parts` surrendered.
    let w = unsafe { MockVec::from_raw_parts(ptr, count) }.unwrap();
    assert!(w.try_clone().is_some());

    drop((v, w));
    // The `try_clone` temporary was freed too.
    assert_eq!(VEC_FREES.load(Ordering::SeqCst), 3);
}

/// # Safety
///
/// `ptr` must be a `mock_vec_alloc` buffer of `byte_len` bytes.
unsafe fn uncounted_vec_free(ptr: *mut u8, byte_len: usize) {
    // SAFETY: caller guarantees the allocation's layout.
    unsafe { std::alloc::dealloc(ptr, vec_layout(byte_len)) }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct UncountedVecFree;
impl_clendrop!(UncountedVecFree, uncounted_vec_free);
/// The mock allocator without a counter, so this test shares no state.
pub type UncountedVec<T> = CVec<T, UncountedVecFree>;

#[test]
fn cvec_of_wrapped_c_types_hands_out_handles() {
    // `bar_st` is `#[repr(C)]` over one `u64`, so an array of payloads is an
    // array of `bar_st`.
    let raw = mock_array(&[4u64, 5]);
    // SAFETY: 2 initialised `bar_st`s; `Bar` is transparent over `bar_st`.
    let v = unsafe { UncountedVec::<Bar>::from_raw_parts(raw.cast(), 2) }.unwrap();
    let handles = v.as_handles();
    // SAFETY: each handle addresses a live `bar_st`.
    let payloads: Vec<u64> = handles
        .iter()
        .map(|h| unsafe { (*h.as_ptr()).payload })
        .collect();
    assert_eq!(payloads, [4, 5]);
}

#[test]
fn cvec_hand_written_policy() {
    // SAFETY: 2 initialised `u8`s from the mock allocator.
    let v =
        unsafe { TaggedVec::from_raw_parts_with(mock_array(&[7u8, 8]), 2, VecTagged(4)) }.unwrap();
    assert_eq!(v.as_slice(), &[7, 8]);
    let (ptr, count, VecTagged(tag)) = v.into_raw_parts_with();
    assert_eq!(tag, 4);
    // SAFETY: re-adopt what `into_raw_parts_with` surrendered.
    drop(unsafe { TaggedVec::from_raw_parts_with(ptr, count, VecTagged(2)) });
    assert_eq!(
        TAGGED_VEC_FREES.load(Ordering::SeqCst),
        2,
        "the re-adopting policy ran"
    );
}

// ---------------------------------------------------------------------------
// CSlice / CSliceMut — one run view for every source
// ---------------------------------------------------------------------------

/// A C struct carrying an inline array and its count — the run lives inside
/// the object, not in a Rust-owned buffer.
#[repr(C)]
pub struct holder_st {
    items: [bar_st; 3],
    nb_items: usize,
    scores: [u32; 3],
}

define_ctype!(Holder, HolderRef, HolderMut, holder_st);

impl HolderRef<'_> {
    /// The items run, borrowed from the holder.
    fn items(&self) -> CSlice<'_, Bar> {
        // SAFETY: `nb_items` initialised `bar_st` at `items`, alive for as long
        // as this handle's borrow; `Bar` is transparent over `bar_st`.
        unsafe {
            let base = core::ptr::addr_of_mut!((*self.as_ptr().cast_mut()).items).cast::<Bar>();
            CSlice::from_raw_parts(
                core::ptr::NonNull::new_unchecked(base),
                (*self.as_ptr()).nb_items,
            )
        }
    }
}

impl HolderMut<'_> {
    /// The scores run, exclusively.
    fn scores_mut(&mut self) -> CSliceMut<'_, u32> {
        // SAFETY: three initialised `u32` inside the holder, exclusively
        // borrowed through this handle.
        unsafe {
            let base = core::ptr::addr_of_mut!((*self.as_mut_ptr()).scores).cast::<u32>();
            CSliceMut::from_raw_parts(core::ptr::NonNull::new_unchecked(base), 3)
        }
    }
}

/// Accepts a run from any source: a buffer's `as_handles` or a struct field.
fn payload_sum(run: CSlice<'_, Bar>) -> u64 {
    // SAFETY: each handle addresses a live `bar_st`.
    run.iter().map(|h| unsafe { (*h.as_ptr()).payload }).sum()
}

fn holder(nb_items: usize) -> holder_st {
    holder_st {
        items: [
            bar_st { payload: 1 },
            bar_st { payload: 2 },
            bar_st { payload: 3 },
        ],
        nb_items,
        scores: [0; 3],
    }
}

#[test]
fn one_view_type_serves_buffers_and_struct_fields() {
    let mut raw = holder(2);
    // SAFETY: `raw` is a live, initialised `holder_st` that outlives the handle.
    let h = unsafe { HolderRef::from_ptr(&raw mut raw) }.unwrap();
    let from_field = h.items();
    assert_eq!(from_field.len(), 2);
    assert_eq!(payload_sum(from_field), 3);

    // SAFETY: 2 initialised `bar_st`s; `Bar` is transparent over `bar_st`.
    let buffer =
        unsafe { UncountedVec::<Bar>::from_raw_parts(mock_array(&[10u64, 20]).cast(), 2) }.unwrap();
    assert_eq!(payload_sum(buffer.as_handles()), 30);
}

#[test]
fn the_exclusive_view_writes_plain_values_in_place() {
    let mut raw = holder(3);
    // SAFETY: as above, and no other handle to `raw` exists.
    let mut h = unsafe { HolderMut::from_ptr(&raw mut raw) }.unwrap();
    let mut scores = h.scores_mut();
    assert!(scores.set_elem(1, 7));
    assert!(!scores.set_elem(3, 9));
    assert!(scores.copy_from_slice(&[4, 5, 6]));
    let shared = scores.as_ref();
    assert_eq!(shared.elems().collect::<Vec<_>>(), [4, 5, 6]);
    assert_eq!(shared.elem(2), Some(6));
    let mut out = [0u32; 3];
    assert!(shared.copy_to_slice(&mut out));
    assert_eq!(out, [4, 5, 6]);
    assert_eq!(raw.scores, [4, 5, 6]);
}

#[test]
fn the_shared_view_is_copy() {
    let mut raw = holder(3);
    // SAFETY: as above.
    let h = unsafe { HolderRef::from_ptr(&raw mut raw) }.unwrap();
    let a = h.items();
    let b = a; // `Copy`, like `&[T]`
    assert_eq!(a.len(), b.len());
    assert!(a.get(3).is_none());
    assert!(!format!("{a:?}").is_empty());
}

// ---------------------------------------------------------------------------
// define_ctype! — handles to a value Rust owns inline
// ---------------------------------------------------------------------------

impl BarRef<'_> {
    fn payload(&self) -> u64 {
        // SAFETY: read through the handle's pointer; no reference is formed.
        unsafe { core::ptr::addr_of!((*self.as_ptr()).payload).read() }
    }
}

impl BarMut<'_> {
    fn set_payload(&mut self, v: u64) {
        // SAFETY: write through the exclusive handle's pointer.
        unsafe { core::ptr::addr_of_mut!((*self.as_mut_ptr()).payload).write(v) }
    }
}

#[test]
fn an_inline_value_reaches_its_handles_without_a_wrapper() {
    // A resource-free value needs neither `CVal` nor an owner: `Bar` itself is
    // the inline storage, and `as_ref` / `as_mut` hand out its handles.
    let mut b = Bar::zeroed();
    assert_eq!(b.as_ref().payload(), 0);
    b.as_mut().set_payload(9);
    assert_eq!(b.as_ref().payload(), 9);

    // Moving the value moves the bytes; the handles are re-derived from it.
    let moved = b;
    assert_eq!(moved.as_ref().payload(), 9);
}
