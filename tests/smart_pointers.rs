//! Integration tests for ffibox's smart pointers and macros.

#![allow(missing_docs)]
//!
//! Tests use mock "C" types backed by [`AtomicUsize`] counters to verify
//! that:
//!
//! - a `CBox` calls its policy's `c_drop` unconditionally on drop, and
//!   `c_dup` on clone; a refcount `up_ref` (`CRefClone`) is settled by the
//!   down-ref it is paired with, and never makes a `CBox` cloneable
//! - one C type may carry several boxes, each running only its own destructor
//! - `CBox::with_policy` promotes a construction-phase allocation
//! - a `CVec` calls the policy's `c_drop_len` with the correct byte length
//! - `into_raw` suppresses cleanup
//! - layout invariants hold (a box over a ZST policy is pointer-sized with
//!   the null niche, a stateful policy makes it fat, etc.)

use core::cell::Cell;
use core::ffi::c_void;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};

use ffibox::{
    impl_cdrop, impl_cdrop_void, impl_cdupclone, impl_clendrop, impl_crefclone, CBorrowedPtr, CBox,
    CCell, CDrop, CRefClone, CSlice, CSliceMut, CVec, CVoidBox,
};

// ---------------------------------------------------------------------------
// Test isolation
// ---------------------------------------------------------------------------
//
// The mock C lifecycle functions record into process-global counters, and each
// test resets its group's counters before exercising them. `cargo test` runs
// tests on a thread pool, so tests sharing a counter group must not overlap —
// otherwise one test's reset clobbers another's in-flight tally. Each group
// below takes its group lock for the duration of the test.
//
// Poisoning is ignored deliberately: an assertion failure in one test should
// surface as that one failure, not cascade into every other test in the group.

static REFCOUNTED_LOCK: Mutex<()> = Mutex::new(());
static BOXED_LOCK: Mutex<()> = Mutex::new(());
static DUPABLE_LOCK: Mutex<()> = Mutex::new(());
static CVEC_LOCK: Mutex<()> = Mutex::new(());
static CVOIDBOX_LOCK: Mutex<()> = Mutex::new(());
static SHARED_LOCK: Mutex<()> = Mutex::new(());
static BUILT_LOCK: Mutex<()> = Mutex::new(());

fn lock(m: &'static Mutex<()>) -> MutexGuard<'static, ()> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---------------------------------------------------------------------------
// Refcounted mock — a policy whose c_drop is the down-ref and c_up_ref the up_ref
// ---------------------------------------------------------------------------

static REFCOUNTED_UP_REF_CALLS: AtomicUsize = AtomicUsize::new(0);
static REFCOUNTED_DOWN_REF_CALLS: AtomicUsize = AtomicUsize::new(0);
static REFCOUNTED_FREED: AtomicUsize = AtomicUsize::new(0);

#[repr(C)]
struct Refcounted {
    /// Real C refcount field. `Cell` because we mutate through `&self`
    /// (interior mutability) to mirror how C code mutates through shared
    /// pointers.
    rc: Cell<usize>,
}

/// Mock `REFCOUNTED_up_ref(p)` — increments the refcount in place.
///
/// # Safety
///
/// `p` must point to a live `Refcounted`.
unsafe fn refcounted_up_ref(p: *mut Refcounted) {
    REFCOUNTED_UP_REF_CALLS.fetch_add(1, Ordering::SeqCst);
    // SAFETY: caller guarantees `p` is live.
    let this = unsafe { &*p };
    this.rc.set(this.rc.get() + 1);
}

/// Mock `REFCOUNTED_free(p)` — decrements the refcount, frees on zero.
///
/// # Safety
///
/// `p` must point to a live `Refcounted` owning one reference.
unsafe fn refcounted_down_ref(p: *mut Refcounted) {
    REFCOUNTED_DOWN_REF_CALLS.fetch_add(1, Ordering::SeqCst);
    // SAFETY: caller guarantees `p` is live.
    let this = unsafe { &*p };
    let new = this.rc.get() - 1;
    this.rc.set(new);
    if new == 0 {
        REFCOUNTED_FREED.fetch_add(1, Ordering::SeqCst);
        // SAFETY: refcount dropped to zero — reclaim the box we leaked in
        // `make_refcounted`.
        drop(unsafe { Box::from_raw(p) });
    }
}

// A refcounted object's sole reference: the down-ref is the destructor. The
// up_ref is registered too, but a `CBox` never clones through it.
#[derive(Clone, Copy, Debug, Default)]
struct RefcountedUnref;
impl_cdrop!(RefcountedUnref, Refcounted, refcounted_down_ref);
impl_crefclone!(RefcountedUnref, Refcounted, refcounted_up_ref);
type RefcountedOwned = CBox<Refcounted, RefcountedUnref>;

fn make_refcounted() -> RefcountedOwned {
    let leaked = Box::into_raw(Box::new(Refcounted { rc: Cell::new(1) }));
    // SAFETY: `leaked` is non-null and represents one outstanding refcount.
    unsafe { RefcountedOwned::from_raw(leaked) }.unwrap()
}

#[test]
fn refcounted_up_ref_is_settled_by_the_down_ref() {
    let _guard = lock(&REFCOUNTED_LOCK);
    REFCOUNTED_UP_REF_CALLS.store(0, Ordering::SeqCst);
    REFCOUNTED_DOWN_REF_CALLS.store(0, Ordering::SeqCst);
    REFCOUNTED_FREED.store(0, Ordering::SeqCst);

    let a = make_refcounted();
    assert_eq!(a.as_ref().rc().get(), 1);

    // A shared owner would call the up_ref on clone; here it is called
    // directly, and the extra reference settled by the paired down-ref.
    let p = NonNull::new(a.as_ptr()).unwrap();
    // SAFETY: `a` keeps the object live; the bump is settled below.
    assert!(unsafe { a.policy().c_up_ref(p) });
    assert_eq!(REFCOUNTED_UP_REF_CALLS.load(Ordering::SeqCst), 1);
    assert_eq!(a.as_ref().rc().get(), 2);

    // SAFETY: settles the reference taken above; `a` still holds its own.
    unsafe { a.policy().c_drop(p) };
    assert_eq!(REFCOUNTED_FREED.load(Ordering::SeqCst), 0);
    drop(a);
    assert_eq!(REFCOUNTED_DOWN_REF_CALLS.load(Ordering::SeqCst), 2);
    assert_eq!(REFCOUNTED_FREED.load(Ordering::SeqCst), 1);
}

/// Compiles only when `T` is NOT `Clone`: for a `Clone` type both impls apply
/// and the `_` inference is ambiguous.
trait AmbiguousIfClone<A> {
    fn check() {}
}
impl<T> AmbiguousIfClone<()> for T {}
impl<T: Clone> AmbiguousIfClone<u8> for T {}

#[test]
fn an_up_ref_does_not_make_a_box_cloneable() {
    // A cloned `CBox` must stay the sole owner, so only `CDupClone` clones it.
    <RefcountedOwned as AmbiguousIfClone<_>>::check();
}

#[test]
fn refcounted_into_raw_preserves_refcount() {
    let _guard = lock(&REFCOUNTED_LOCK);
    REFCOUNTED_DOWN_REF_CALLS.store(0, Ordering::SeqCst);
    REFCOUNTED_FREED.store(0, Ordering::SeqCst);

    let a = make_refcounted();
    let raw = a.into_raw();
    assert_eq!(REFCOUNTED_DOWN_REF_CALLS.load(Ordering::SeqCst), 0);

    // SAFETY: `raw` came from `into_raw` and represents one refcount.
    let restored = unsafe { RefcountedOwned::from_raw(raw) }.unwrap();
    drop(restored);
    assert_eq!(REFCOUNTED_DOWN_REF_CALLS.load(Ordering::SeqCst), 1);
    assert_eq!(REFCOUNTED_FREED.load(Ordering::SeqCst), 1);
}

#[test]
fn refcounted_from_raw_null_returns_none() {
    // SAFETY: passing null is explicitly the documented `None` case.
    let v = unsafe { RefcountedOwned::from_raw(core::ptr::null_mut()) };
    assert!(v.is_none());
}

// ---------------------------------------------------------------------------
// A hand-written `CRefClone` whose up_ref can fail, over a hand-written policy
// ---------------------------------------------------------------------------

/// A refcounted mock that simulates refcount overflow: `c_up_ref` reports
/// failure once the counter reaches `SATURATING_MAX`, replicating what a real C
/// `*_up_ref` reports on integer overflow. The policy is written by hand, to
/// exercise `CRefClone` directly; `impl_crefclone!`'s `ok` form covers the same
/// shape (see `tests/shared.rs`).
const SATURATING_MAX: usize = 3;

#[repr(C)]
struct Saturating {
    rc: Cell<usize>,
}

#[derive(Clone)]
struct SaturatingUnref;

// SAFETY: `c_drop` decrements the refcount and reclaims the Box on zero.
unsafe impl CDrop<Saturating> for SaturatingUnref {
    unsafe fn c_drop(&self, ptr: NonNull<Saturating>) {
        // SAFETY: caller upholds the CDrop::c_drop contract.
        let this = unsafe { ptr.as_ref() };
        let new = this.rc.get() - 1;
        this.rc.set(new);
        if new == 0 {
            // SAFETY: refcount zero — reclaim the Box.
            drop(unsafe { Box::from_raw(ptr.as_ptr()) });
        }
    }
}

// SAFETY: `c_up_ref` reports `false` on overflow (simulating INT_MAX
// exceeded) without touching the count, and otherwise increments it.
unsafe impl CRefClone<Saturating> for SaturatingUnref {
    unsafe fn c_up_ref(&self, ptr: NonNull<Saturating>) -> bool {
        // SAFETY: caller upholds the CRefClone::c_up_ref contract.
        let this = unsafe { ptr.as_ref() };
        if this.rc.get() >= SATURATING_MAX {
            return false; // simulate refcount overflow
        }
        this.rc.set(this.rc.get() + 1);
        true
    }
}

type SaturatingOwned = CBox<Saturating, SaturatingUnref>;

fn make_saturating() -> SaturatingOwned {
    let leaked = Box::into_raw(Box::new(Saturating { rc: Cell::new(1) }));
    // SAFETY: non-null, represents one refcount that `SaturatingUnref` settles.
    unsafe { SaturatingOwned::from_raw_with(leaked, SaturatingUnref) }.unwrap()
}

#[test]
fn a_failing_up_ref_reports_false_and_changes_nothing() {
    let a = make_saturating();
    let p = NonNull::new(a.as_ptr()).unwrap();
    // SAFETY: `a` keeps the object live; each successful bump is settled below.
    unsafe {
        assert!(a.policy().c_up_ref(p)); // rc = 2
        assert!(a.policy().c_up_ref(p)); // rc = 3 = SATURATING_MAX
        assert!(!a.policy().c_up_ref(p), "overflow must report failure");
    }
    assert_eq!(a.as_ref().rc().get(), SATURATING_MAX);

    // SAFETY: settles the two successful bumps; `a` keeps its own reference.
    unsafe {
        a.policy().c_drop(p);
        a.policy().c_drop(p);
    }
    assert_eq!(a.as_ref().rc().get(), 1);
}

#[test]
fn hand_written_policy_round_trips_through_into_raw_with() {
    let a = make_saturating();
    let (raw, policy) = a.into_raw_with();
    // SAFETY: `raw` and its policy came from `into_raw_with` and still own
    // one ref.
    let a = unsafe { SaturatingOwned::from_raw_with(raw, policy) }.unwrap();
    assert_eq!(a.as_ref().rc().get(), 1);
}

// ---------------------------------------------------------------------------
// Unique-owned mock
// ---------------------------------------------------------------------------

static BOXED_FREE_CALLS: AtomicUsize = AtomicUsize::new(0);

#[repr(C)]
struct Boxed {
    payload: u32,
    /// Internal teardown gate. The owner always calls `c_drop`; the decision
    /// to actually reclaim folds INTO the free routine (the recommended
    /// pattern). `false` leaves the storage for the caller.
    should_free: bool,
}

/// Mock `BOXED_free(p)`, gated on the object's own `should_free`.
///
/// # Safety
///
/// `p` must point to a live, uniquely-owned `Boxed`.
unsafe fn boxed_free(p: *mut Boxed) {
    BOXED_FREE_CALLS.fetch_add(1, Ordering::SeqCst);
    // SAFETY: caller guarantees `p` is live. The gate lives here: only
    // reclaim the leaked Box when the object asks for it.
    if unsafe { (*p).should_free } {
        // SAFETY: as above; `p` came from `Box::into_raw`.
        drop(unsafe { Box::from_raw(p) });
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct BoxedFree;
impl_cdrop!(BoxedFree, Boxed, boxed_free);
type BoxedOwned = CBox<Boxed, BoxedFree>;

fn make_boxed(should_free: bool) -> BoxedOwned {
    let leaked = Box::into_raw(Box::new(Boxed {
        payload: 42,
        should_free,
    }));
    // SAFETY: `leaked` is non-null and uniquely owned.
    unsafe { BoxedOwned::from_raw(leaked) }.unwrap()
}

#[test]
fn owned_drop_calls_c_drop() {
    let _guard = lock(&BOXED_LOCK);
    BOXED_FREE_CALLS.store(0, Ordering::SeqCst);

    let b = make_boxed(true);
    assert_eq!(b.as_ref().payload(), 42);
    drop(b);
    assert_eq!(BOXED_FREE_CALLS.load(Ordering::SeqCst), 1);
}

#[test]
fn owned_always_calls_c_drop_gate_folds_internally() {
    let _guard = lock(&BOXED_LOCK);
    BOXED_FREE_CALLS.store(0, Ordering::SeqCst);

    // `should_free = false`: the owner still calls c_drop unconditionally, but
    // the gate inside the free routine declines to reclaim, leaving the
    // storage to us.
    let b = make_boxed(false);
    let raw = b.as_ptr();
    drop(b);
    assert_eq!(
        BOXED_FREE_CALLS.load(Ordering::SeqCst),
        1,
        "drop must call c_drop unconditionally; the skip gate lives inside c_drop"
    );
    // Reclaim the leaked box (the internal gate declined to).
    // SAFETY: `raw` was the only outstanding pointer and the free routine did
    // not free it (should_free = false).
    drop(unsafe { Box::from_raw(raw) });
}

#[test]
fn owned_into_raw_suppresses_drop() {
    let _guard = lock(&BOXED_LOCK);
    BOXED_FREE_CALLS.store(0, Ordering::SeqCst);

    let b = make_boxed(true);
    let raw = b.into_raw();
    assert_eq!(BOXED_FREE_CALLS.load(Ordering::SeqCst), 0);

    // SAFETY: `raw` came from `into_raw` and is still uniquely owned.
    let restored = unsafe { BoxedOwned::from_raw(raw) }.unwrap();
    drop(restored);
    assert_eq!(BOXED_FREE_CALLS.load(Ordering::SeqCst), 1);
}

// ---------------------------------------------------------------------------
// `impl_cdupclone!` — opt-in deep clone
// ---------------------------------------------------------------------------

static DUPABLE_FREE_CALLS: AtomicUsize = AtomicUsize::new(0);
static DUPABLE_DUP_CALLS: AtomicUsize = AtomicUsize::new(0);
/// When non-zero, the next dup returns NULL and decrements this counter — used
/// to drive the fallible path without races between tests (each test resets it
/// explicitly).
static DUPABLE_DUP_FAILURES: AtomicUsize = AtomicUsize::new(0);

#[repr(C)]
struct Dupable {
    payload: u32,
}

/// Mock `DUPABLE_free(p)`.
///
/// # Safety
///
/// `p` must point to a live, uniquely-owned `Dupable` from `Box::into_raw`.
unsafe fn dupable_free(p: *mut Dupable) {
    DUPABLE_FREE_CALLS.fetch_add(1, Ordering::SeqCst);
    // SAFETY: caller upholds the contract; reclaim the leaked Box.
    drop(unsafe { Box::from_raw(p) });
}

/// Mock `DUPABLE_dup(p)` — a brand-new owned Box independent of `p`, or NULL
/// when the test has armed the failure counter. Releasable by `dupable_free`.
///
/// # Safety
///
/// `p` must point to a live `Dupable`.
unsafe fn dupable_dup(p: *mut Dupable) -> *mut Dupable {
    DUPABLE_DUP_CALLS.fetch_add(1, Ordering::SeqCst);
    if DUPABLE_DUP_FAILURES.load(Ordering::SeqCst) > 0 {
        DUPABLE_DUP_FAILURES.fetch_sub(1, Ordering::SeqCst);
        return core::ptr::null_mut();
    }
    // SAFETY: caller guarantees `p` is live.
    let payload = unsafe { (*p).payload };
    Box::into_raw(Box::new(Dupable { payload }))
}

#[derive(Clone, Copy, Debug, Default)]
struct DupableFree;
impl_cdrop!(DupableFree, Dupable, dupable_free);
impl_cdupclone!(DupableFree, Dupable, dupable_dup);
type DupableOwned = CBox<Dupable, DupableFree>;

fn make_dupable(payload: u32) -> DupableOwned {
    let leaked = Box::into_raw(Box::new(Dupable { payload }));
    // SAFETY: `leaked` is non-null and uniquely owned.
    unsafe { DupableOwned::from_raw(leaked) }.unwrap()
}

#[test]
fn owned_clone_invokes_dup_and_produces_independent_handle() {
    let _guard = lock(&DUPABLE_LOCK);
    DUPABLE_FREE_CALLS.store(0, Ordering::SeqCst);
    DUPABLE_DUP_CALLS.store(0, Ordering::SeqCst);
    DUPABLE_DUP_FAILURES.store(0, Ordering::SeqCst);

    let a = make_dupable(123);
    let b = a.clone();

    assert_eq!(DUPABLE_DUP_CALLS.load(Ordering::SeqCst), 1);
    // Distinct allocations — deep clone, not refcount bump.
    assert_ne!(a.as_ptr(), b.as_ptr());
    assert_eq!(a.as_ref().payload(), 123);
    assert_eq!(b.as_ref().payload(), 123);

    // Each handle owns its own allocation: dropping both calls c_drop twice.
    drop(a);
    drop(b);
    assert_eq!(DUPABLE_FREE_CALLS.load(Ordering::SeqCst), 2);
}

#[test]
fn owned_try_clone_succeeds_returns_some() {
    let _guard = lock(&DUPABLE_LOCK);
    DUPABLE_FREE_CALLS.store(0, Ordering::SeqCst);
    DUPABLE_DUP_CALLS.store(0, Ordering::SeqCst);
    DUPABLE_DUP_FAILURES.store(0, Ordering::SeqCst);

    let a = make_dupable(7);
    let b = a.try_clone().expect("dup success path must yield Some");
    assert_eq!(b.as_ref().payload(), 7);
    assert_ne!(a.as_ptr(), b.as_ptr());

    drop(a);
    drop(b);
    assert_eq!(DUPABLE_FREE_CALLS.load(Ordering::SeqCst), 2);
}

#[test]
fn owned_try_clone_returns_none_on_dup_failure() {
    let _guard = lock(&DUPABLE_LOCK);
    DUPABLE_FREE_CALLS.store(0, Ordering::SeqCst);
    DUPABLE_DUP_CALLS.store(0, Ordering::SeqCst);
    DUPABLE_DUP_FAILURES.store(1, Ordering::SeqCst);

    let a = make_dupable(99);
    let attempt = a.try_clone();
    assert!(attempt.is_none(), "try_clone must propagate a NULL dup");
    // Original handle is still live and untouched.
    assert_eq!(a.as_ref().payload(), 99);

    drop(a);
    // Only the original is freed — failed clone did not produce a handle.
    assert_eq!(DUPABLE_FREE_CALLS.load(Ordering::SeqCst), 1);
}

// ---------------------------------------------------------------------------
// One C type, two boxes: each runs its own destructor, and each clone
// routine is settled by the teardown it was paired with.
// ---------------------------------------------------------------------------

static SHARED_UP_REF_CALLS: AtomicUsize = AtomicUsize::new(0);
static SHARED_UNREF_CALLS: AtomicUsize = AtomicUsize::new(0);
static SHARED_DUP_CALLS: AtomicUsize = AtomicUsize::new(0);
static SHARED_FREE_CALLS: AtomicUsize = AtomicUsize::new(0);

/// A C type exposing both a refcount pair (`up_ref` / `unref`) and a
/// deep-copy pair (`dup` / `free`). `free` ignores the count and reclaims
/// outright; `unref` reclaims on zero.
#[repr(C)]
struct Shared {
    rc: Cell<usize>,
    payload: u32,
}

/// # Safety
///
/// `p` must point to a live `Shared`.
unsafe fn shared_up_ref(p: *mut Shared) -> i32 {
    SHARED_UP_REF_CALLS.fetch_add(1, Ordering::SeqCst);
    // SAFETY: caller guarantees `p` is live.
    let this = unsafe { &*p };
    this.rc.set(this.rc.get() + 1);
    1
}

/// # Safety
///
/// `p` must point to a live `Shared` owning one reference.
unsafe fn shared_unref(p: *mut Shared) {
    SHARED_UNREF_CALLS.fetch_add(1, Ordering::SeqCst);
    // SAFETY: caller guarantees `p` is live.
    let this = unsafe { &*p };
    let new = this.rc.get() - 1;
    this.rc.set(new);
    if new == 0 {
        // SAFETY: last reference — reclaim the Box.
        drop(unsafe { Box::from_raw(p) });
    }
}

/// # Safety
///
/// `p` must point to a live `Shared`.
unsafe fn shared_dup(p: *mut Shared) -> *mut Shared {
    SHARED_DUP_CALLS.fetch_add(1, Ordering::SeqCst);
    // SAFETY: caller guarantees `p` is live.
    let payload = unsafe { (*p).payload };
    Box::into_raw(Box::new(Shared {
        rc: Cell::new(1),
        payload,
    }))
}

/// # Safety
///
/// `p` must point to a live, sole-owned `Shared` from `Box::into_raw`.
unsafe fn shared_free(p: *mut Shared) {
    SHARED_FREE_CALLS.fetch_add(1, Ordering::SeqCst);
    // SAFETY: caller guarantees sole ownership; reclaim the Box.
    drop(unsafe { Box::from_raw(p) });
}

// Two policies over one C type, so two box types.
#[derive(Clone, Copy, Debug, Default)]
struct SharedUnref;
impl_cdrop!(SharedUnref, Shared, shared_unref);
impl_crefclone!(SharedUnref, Shared, shared_up_ref, ok = |r| r == 1);
/// The sole reference to a refcounted object: `unref` settles each `up_ref`.
type SharedRef = CBox<Shared, SharedUnref>;

#[derive(Clone, Copy, Debug, Default)]
struct SharedFree;
impl_cdrop!(SharedFree, Shared, shared_free);
impl_cdupclone!(SharedFree, Shared, shared_dup);
/// A sole owner: `free` settles each `dup`.
type SharedCopy = CBox<Shared, SharedFree>;

fn make_shared(payload: u32) -> *mut Shared {
    Box::into_raw(Box::new(Shared {
        rc: Cell::new(1),
        payload,
    }))
}

fn reset_shared() {
    for c in [
        &SHARED_UP_REF_CALLS,
        &SHARED_UNREF_CALLS,
        &SHARED_DUP_CALLS,
        &SHARED_FREE_CALLS,
    ] {
        c.store(0, Ordering::SeqCst);
    }
}

#[test]
fn two_owned_newtypes_run_their_own_destructor() {
    let _guard = lock(&SHARED_LOCK);
    reset_shared();

    // SAFETY: fresh allocation with rc = 1, owned by the refcount pair.
    let r = unsafe { SharedRef::from_raw(make_shared(1)) }.unwrap();
    // SAFETY: fresh allocation, sole-owned, released by `shared_free`.
    let c = unsafe { SharedCopy::from_raw(make_shared(2)) }.unwrap();

    drop(r);
    assert_eq!(SHARED_UNREF_CALLS.load(Ordering::SeqCst), 1);
    assert_eq!(SHARED_FREE_CALLS.load(Ordering::SeqCst), 0);

    drop(c);
    assert_eq!(SHARED_UNREF_CALLS.load(Ordering::SeqCst), 1);
    assert_eq!(SHARED_FREE_CALLS.load(Ordering::SeqCst), 1);
}

#[test]
fn up_ref_pairs_with_unref_and_dup_with_free() {
    let _guard = lock(&SHARED_LOCK);
    reset_shared();

    // Refcount pair: an up_ref is the same object, settled by a second unref.
    // SAFETY: fresh allocation with rc = 1.
    let r = unsafe { SharedRef::from_raw(make_shared(5)) }.unwrap();
    let p = NonNull::new(r.as_ptr()).unwrap();
    // SAFETY: `r` keeps the object live; the bump is settled just below.
    assert!(unsafe { r.policy().c_up_ref(p) });
    assert_eq!(r.as_ref().rc().get(), 2);
    assert_eq!(SHARED_UP_REF_CALLS.load(Ordering::SeqCst), 1);
    assert_eq!(SHARED_DUP_CALLS.load(Ordering::SeqCst), 0);
    // SAFETY: settles the reference taken above.
    unsafe { r.policy().c_drop(p) };
    drop(r);
    assert_eq!(SHARED_UNREF_CALLS.load(Ordering::SeqCst), 2);
    assert_eq!(SHARED_FREE_CALLS.load(Ordering::SeqCst), 0);

    // Deep-copy pair: a clone is a new object, settled by a second free.
    // SAFETY: fresh allocation, sole-owned.
    let c1 = unsafe { SharedCopy::from_raw(make_shared(6)) }.unwrap();
    let c2 = c1.clone();
    assert_ne!(c1.as_ptr(), c2.as_ptr());
    assert_eq!(c2.as_ref().payload(), 6);
    assert_eq!(SHARED_DUP_CALLS.load(Ordering::SeqCst), 1);
    assert_eq!(SHARED_UP_REF_CALLS.load(Ordering::SeqCst), 1);
    drop(c1);
    drop(c2);
    assert_eq!(SHARED_FREE_CALLS.load(Ordering::SeqCst), 2);
    assert_eq!(SHARED_UNREF_CALLS.load(Ordering::SeqCst), 2);
}

// ---------------------------------------------------------------------------
// Construction phase — hold under a storage-only policy, then `with_policy`
// ---------------------------------------------------------------------------

static BUILT_STORAGE_FREES: AtomicUsize = AtomicUsize::new(0);
static BUILT_FULL_FREES: AtomicUsize = AtomicUsize::new(0);

/// A C object whose real destructor reads a sub-allocation that only exists
/// once construction finishes.
#[repr(C)]
struct Built {
    finished: bool,
}

/// # Safety
///
/// `p` must point to a live, fully-built `Built` from `Box::into_raw`.
unsafe fn built_free(p: *mut Built) {
    // SAFETY: caller guarantees `p` is live.
    let finished = unsafe { (*p).finished };
    assert!(finished, "the full destructor saw a half-built object");
    BUILT_FULL_FREES.fetch_add(1, Ordering::SeqCst);
    // SAFETY: as above; reclaim the Box.
    drop(unsafe { Box::from_raw(p) });
}

#[derive(Clone, Copy, Debug, Default)]
struct BuiltFree;
impl_cdrop!(BuiltFree, Built, built_free);
type BuiltOwned = CBox<Built, BuiltFree>;

/// Storage-only teardown: reclaims the allocation without touching fields.
struct BuiltStorageFree;

// SAFETY: frees the Box backing the allocation exactly once and reads nothing.
unsafe impl CDrop<Built> for BuiltStorageFree {
    unsafe fn c_drop(&self, ptr: NonNull<Built>) {
        BUILT_STORAGE_FREES.fetch_add(1, Ordering::SeqCst);
        // SAFETY: caller upholds the contract; `ptr` came from `Box::into_raw`.
        drop(unsafe { Box::from_raw(ptr.as_ptr()) });
    }
}

fn begin_built() -> CBox<Built, BuiltStorageFree> {
    let raw = Box::into_raw(Box::new(Built { finished: false }));
    // SAFETY: fresh allocation, released by the storage-only policy.
    unsafe { CBox::from_raw_with(raw, BuiltStorageFree) }.unwrap()
}

#[test]
fn bailing_before_promotion_runs_only_the_storage_free() {
    let _guard = lock(&BUILT_LOCK);
    BUILT_STORAGE_FREES.store(0, Ordering::SeqCst);
    BUILT_FULL_FREES.store(0, Ordering::SeqCst);

    let half = begin_built();
    drop(half); // the `?` path
    assert_eq!(BUILT_STORAGE_FREES.load(Ordering::SeqCst), 1);
    assert_eq!(BUILT_FULL_FREES.load(Ordering::SeqCst), 0);
}

#[test]
fn with_policy_promotes_to_the_full_destructor() {
    let _guard = lock(&BUILT_LOCK);
    BUILT_STORAGE_FREES.store(0, Ordering::SeqCst);
    BUILT_FULL_FREES.store(0, Ordering::SeqCst);

    let half = begin_built();
    // SAFETY: `half` owns the allocation; finishing it in place.
    unsafe { (*half.as_ptr()).finished = true };
    // SAFETY: the object is now fully built, as `built_free` requires.
    let (owned, _storage): (BuiltOwned, _) = unsafe { half.with_policy(BuiltFree) };
    assert_eq!(BUILT_STORAGE_FREES.load(Ordering::SeqCst), 0);

    drop(owned);
    assert_eq!(BUILT_STORAGE_FREES.load(Ordering::SeqCst), 0);
    assert_eq!(BUILT_FULL_FREES.load(Ordering::SeqCst), 1);
}

// ---------------------------------------------------------------------------
// CVec — policy-based buffer cleanup
// ---------------------------------------------------------------------------

static CVEC_CLEANUP_CALLS: AtomicUsize = AtomicUsize::new(0);
static CVEC_LAST_BYTE_LEN: AtomicUsize = AtomicUsize::new(usize::MAX);

/// Element alignment of the live mock buffer, so `recording_free` can free with
/// the layout it was allocated with (a C allocator knows this implicitly; the
/// mock has to record it).
static CVEC_ELEM_ALIGN: AtomicUsize = AtomicUsize::new(0);

/// Cleanup that records each call so tests can verify the byte length passed
/// in, and reclaims the allocation `make_cvec` made.
///
/// # Safety
///
/// `ptr` / `byte_len` must be the allocation `make_cvec` made, whose element
/// alignment is in `CVEC_ELEM_ALIGN`.
unsafe fn recording_free(ptr: *mut u8, byte_len: usize) {
    CVEC_CLEANUP_CALLS.fetch_add(1, Ordering::SeqCst);
    CVEC_LAST_BYTE_LEN.store(byte_len, Ordering::SeqCst);
    // Reconstituting the buffer as `Box<[u8]>` would deallocate with alignment
    // 1 against an allocation of the element's alignment.
    let align = CVEC_ELEM_ALIGN.load(Ordering::SeqCst);
    if byte_len == 0 || align == 0 {
        return;
    }
    // SAFETY: `byte_len` and `align` are the layout `make_cvec` allocated
    // with, and `ptr` is that allocation.
    unsafe {
        std::alloc::dealloc(
            ptr,
            std::alloc::Layout::from_size_align_unchecked(byte_len, align),
        );
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct RecordingFree;
impl_clendrop!(RecordingFree, recording_free);
/// A buffer from the recording mock allocator.
type RecVec<T> = CVec<T, RecordingFree>;

/// Every caller holds `CVEC_LOCK`: the alignment recorded here and the cleanup
/// counters are process-global.
fn make_cvec<T>(elems: Vec<T>) -> RecVec<T> {
    let count = elems.len();
    let layout = std::alloc::Layout::array::<T>(count).expect("layout");
    CVEC_ELEM_ALIGN.store(layout.align(), Ordering::SeqCst);
    // SAFETY: `count > 0` in every test, so the layout is non-zero-sized.
    let ptr = unsafe { std::alloc::alloc(layout) }.cast::<T>();
    assert!(!ptr.is_null());
    for (i, e) in elems.into_iter().enumerate() {
        // SAFETY: `i < count`, writing into freshly allocated storage.
        unsafe { ptr.add(i).write(e) };
    }
    // SAFETY: `ptr` is non-null, `count` matches the allocation.
    unsafe { RecVec::from_raw_parts(ptr, count) }.unwrap()
}

#[test]
fn cvec_basic_slice_view() {
    let _guard = lock(&CVEC_LOCK);
    let v: RecVec<u32> = make_cvec(vec![1u32, 2, 3, 4]);
    assert_eq!(v.len(), 4);
    assert_eq!(v.byte_len(), 16);
    assert!(!v.is_empty());
    assert_eq!(v.as_slice(), &[1, 2, 3, 4]);
}

#[test]
fn cvec_mutable_slice_view() {
    let _guard = lock(&CVEC_LOCK);
    let mut v: RecVec<u8> = make_cvec(vec![0u8, 0, 0]);
    v.as_mut_slice().copy_from_slice(&[10, 20, 30]);
    assert_eq!(v.as_slice(), &[10, 20, 30]);
}

#[test]
fn cvec_drop_calls_cleanup_with_correct_byte_len() {
    let _guard = lock(&CVEC_LOCK);
    CVEC_CLEANUP_CALLS.store(0, Ordering::SeqCst);
    CVEC_LAST_BYTE_LEN.store(usize::MAX, Ordering::SeqCst);

    let v: RecVec<u32> = make_cvec(vec![0u32; 8]);
    let expected_bytes = 8 * core::mem::size_of::<u32>();
    drop(v);

    assert_eq!(CVEC_CLEANUP_CALLS.load(Ordering::SeqCst), 1);
    assert_eq!(CVEC_LAST_BYTE_LEN.load(Ordering::SeqCst), expected_bytes);
}

#[test]
fn cvec_into_raw_parts_suppresses_cleanup() {
    let _guard = lock(&CVEC_LOCK);
    CVEC_CLEANUP_CALLS.store(0, Ordering::SeqCst);

    let v: RecVec<u8> = make_cvec(vec![1u8, 2, 3]);
    let (ptr, count) = v.into_raw_parts();
    assert_eq!(count, 3);
    assert_eq!(CVEC_CLEANUP_CALLS.load(Ordering::SeqCst), 0);

    // SAFETY: `ptr` was just returned from `into_raw_parts`; reclaiming
    // it through a new buffer is sound. Drop will trigger cleanup.
    let restored: RecVec<u8> = unsafe { RecVec::from_raw_parts(ptr, count) }.unwrap();
    drop(restored);
    assert_eq!(CVEC_CLEANUP_CALLS.load(Ordering::SeqCst), 1);
}

// ---------------------------------------------------------------------------
// Layout invariants
// ---------------------------------------------------------------------------

#[test]
fn refcounted_box_is_pointer_sized() {
    assert_eq!(
        core::mem::size_of::<RefcountedOwned>(),
        core::mem::size_of::<*mut Refcounted>(),
    );
    // NonNull niche: Option<RefcountedOwned> stays pointer-sized.
    assert_eq!(
        core::mem::size_of::<Option<RefcountedOwned>>(),
        core::mem::size_of::<*mut Refcounted>(),
    );
}

#[test]
fn owned_is_pointer_sized() {
    assert_eq!(
        core::mem::size_of::<BoxedOwned>(),
        core::mem::size_of::<*mut Boxed>(),
    );
    assert_eq!(
        core::mem::size_of::<Option<BoxedOwned>>(),
        core::mem::size_of::<*mut Boxed>(),
    );
}

/// A policy carrying runtime state, to contrast with the ZST ones.
struct StatefulFree(#[allow(dead_code)] usize);

// SAFETY: never invoked — the type is only measured.
unsafe impl CDrop<Boxed> for StatefulFree {
    unsafe fn c_drop(&self, _: NonNull<Boxed>) {}
}

#[test]
fn zst_policy_keeps_the_storage_pointer_sized() {
    use core::mem::{align_of, size_of};

    // Every macro-bound policy is a ZST, so the box is exactly a pointer with
    // the null niche.
    assert_eq!(size_of::<BoxedFree>(), 0);
    assert_eq!(size_of::<RefcountedUnref>(), 0);
    assert_eq!(size_of::<SaturatingUnref>(), 0);
    assert_eq!(size_of::<CBox<Boxed, BoxedFree>>(), size_of::<*mut Boxed>());
    assert_eq!(
        align_of::<CBox<Boxed, BoxedFree>>(),
        align_of::<*mut Boxed>()
    );
    assert_eq!(
        size_of::<Option<CBox<Boxed, BoxedFree>>>(),
        size_of::<*mut Boxed>()
    );
    // A hand-written ZST policy gets the same layout.
    assert_eq!(size_of::<SaturatingOwned>(), size_of::<*mut Saturating>());
    assert_eq!(
        size_of::<Option<SaturatingOwned>>(),
        size_of::<*mut Saturating>()
    );

    // A stateful policy is stored inline, so the owner is genuinely fat.
    assert_eq!(
        size_of::<CBox<Boxed, StatefulFree>>(),
        size_of::<*mut Boxed>() + size_of::<usize>()
    );
}

#[test]
fn cvec_is_ptr_plus_usize() {
    assert_eq!(
        core::mem::size_of::<RecVec<u8>>(),
        core::mem::size_of::<*mut u8>() + core::mem::size_of::<usize>(),
    );
    assert_eq!(
        core::mem::size_of::<CVec<u8, RecordingFree>>(),
        core::mem::size_of::<*mut u8>() + core::mem::size_of::<usize>(),
    );
}

// ---------------------------------------------------------------------------
// `void *` payloads — `CVoidBox`
// ---------------------------------------------------------------------------
//
// Unlike a typed box, a `CVoidBox` keeps the pointee erased
// throughout: only the policy is known. The bytes behind the `void *` are
// never read as a Rust type — they are merely owned and freed. These tests use
// a Rust-allocated blob standing in for a C allocation, freed through a
// C-style free function bound by `impl_cdrop_void!`.

static COWN_FREE_CALLS: AtomicUsize = AtomicUsize::new(0);
static COWN_FREED_PTR: core::sync::atomic::AtomicPtr<c_void> =
    core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

// The opaque allocation hiding behind the `void *`. The owned type never names
// it; only the test's free routine does, to reclaim and drop it.
#[repr(C)]
struct ErasedBlob {
    #[allow(dead_code)]
    payload: u32,
}

/// A C-style destructor (`unsafe extern "C" fn(*mut c_void)`), mirroring a
/// real free routine such as libgit2's `git__free`.
///
/// # Safety
///
/// `ptr` must be an `ErasedBlob` from `Box::into_raw`, owned by the caller.
unsafe extern "C" fn test_blob_free(ptr: *mut c_void) {
    COWN_FREE_CALLS.fetch_add(1, Ordering::SeqCst);
    COWN_FREED_PTR.store(ptr, Ordering::SeqCst);
    // SAFETY: caller upholds the contract; reclaim the Rust-allocated blob
    // standing in for the C allocation.
    drop(unsafe { Box::from_raw(ptr.cast::<ErasedBlob>()) });
}

#[derive(Clone, Copy, Debug, Default)]
struct TestBlobFree;
impl_cdrop_void!(TestBlobFree, test_blob_free);
/// An erased blob freed by `test_blob_free` (cf. a `git__free`d buffer).
type OwnedBlob = CVoidBox<TestBlobFree>;

// Leak an `ErasedBlob` and hand its address out as an opaque `void *`.
fn make_owned_blob(payload: u32) -> (OwnedBlob, *mut c_void) {
    let raw = Box::into_raw(Box::new(ErasedBlob { payload })).cast::<c_void>();
    // SAFETY: `raw` is a fresh, uniquely-owned allocation; `test_blob_free` is
    // its correct destructor.
    (unsafe { OwnedBlob::from_raw(raw) }.unwrap(), raw)
}

#[test]
fn cown_drop_frees_once_via_policy() {
    let _guard = lock(&CVOIDBOX_LOCK);
    COWN_FREE_CALLS.store(0, Ordering::SeqCst);
    COWN_FREED_PTR.store(core::ptr::null_mut(), Ordering::SeqCst);

    let (own, raw) = make_owned_blob(0xABCD);
    assert_eq!(COWN_FREE_CALLS.load(Ordering::SeqCst), 0);

    drop(own);

    assert_eq!(
        COWN_FREE_CALLS.load(Ordering::SeqCst),
        1,
        "Drop must invoke the policy exactly once"
    );
    assert_eq!(
        COWN_FREED_PTR.load(Ordering::SeqCst),
        raw,
        "the destructor must receive the original erased address (no header, no cast drift)"
    );
}

#[test]
fn cown_from_null_is_none() {
    // SAFETY: null is the documented `None` case.
    assert!(unsafe { OwnedBlob::from_raw(core::ptr::null_mut()) }.is_none());
}

#[test]
fn cown_into_raw_then_from_raw_round_trips_without_freeing() {
    let _guard = lock(&CVOIDBOX_LOCK);
    COWN_FREE_CALLS.store(0, Ordering::SeqCst);

    let (own, raw) = make_owned_blob(7);

    // Surrender to a C `void *` slot — must NOT free.
    let foreign = own.into_raw();
    assert_eq!(foreign, raw, "into_raw yields the same erased address");
    assert_eq!(
        COWN_FREE_CALLS.load(Ordering::SeqCst),
        0,
        "into_raw must not run the destructor"
    );

    // Reclaim from the slot — the policy rides along in the type, so no extra
    // data is threaded through C.
    // SAFETY: `foreign` came from `into_raw` on an `OwnedBlob` and has not
    // been consumed since.
    let own = unsafe { OwnedBlob::from_raw(foreign) }.unwrap();
    assert_eq!(COWN_FREE_CALLS.load(Ordering::SeqCst), 0);

    drop(own);
    assert_eq!(
        COWN_FREE_CALLS.load(Ordering::SeqCst),
        1,
        "exactly one free after the full round-trip"
    );
}

#[test]
fn cown_is_pointer_sized_voidptr() {
    // Frees a blob, which the counting tests above observe.
    let _guard = lock(&CVOIDBOX_LOCK);
    use core::mem::{align_of, size_of};

    // A ZST policy keeps the layout of a raw `void *`, and `Option<OwnedBlob>`
    // is the null-niche `void *`.
    assert_eq!(size_of::<OwnedBlob>(), size_of::<*mut c_void>());
    assert_eq!(align_of::<OwnedBlob>(), align_of::<*mut c_void>());
    assert_eq!(size_of::<Option<OwnedBlob>>(), size_of::<*mut c_void>());

    // as_ptr / into_raw alias the original allocation exactly.
    let (own, raw) = make_owned_blob(1);
    assert_eq!(own.as_ptr(), raw);
    drop(own);
}

// The mocks play both wrapper and C type: each is its own `C`. The handles are the generic
// mock pair below; the seam is never invoked (the tests project raw pointers
// directly), so these impls only satisfy the bound.

/// Generic mock shared handle — one pointer, `Copy`, like `&T`.
#[repr(transparent)]
pub struct MockRef<'a, T>(CBorrowedPtr<'a, T>);
impl<T> Clone for MockRef<'_, T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for MockRef<'_, T> {}
impl<T> MockRef<'_, T> {
    fn as_ptr(&self) -> *mut T {
        self.0.as_non_null().as_ptr()
    }
}

/// Generic mock exclusive handle — move-only.
#[repr(transparent)]
pub struct MockMut<'a, T>(MockRef<'a, T>);
impl<T> MockMut<'_, T> {
    #[allow(dead_code)]
    fn as_mut_ptr(&mut self) -> *mut T {
        self.0 .0.as_non_null().as_ptr()
    }
}

macro_rules! mock_ccell {
    ($($t:ty),* $(,)?) => {$(
        // SAFETY: the `#[repr(C)]` mock is its own `C`, so trivially layout-compatible;
        // the handles are transparent over `CBorrowedPtr` and expose no
        // reference to it.
        unsafe impl CCell for $t {
            type C = $t;
            type Ref<'a> = MockRef<'a, $t> where Self: 'a;
            type Mut<'a> = MockMut<'a, $t> where Self: 'a;
        }
    )*};
}

mock_ccell!(Refcounted, Saturating, Boxed, Dupable, Shared, Built);

macro_rules! mock_getter {
    ($($t:ty => $field:ident : $ret:ty),* $(,)?) => {$(
        impl MockRef<'_, $t> {
            fn $field(&self) -> $ret {
                // SAFETY: the handle borrows a live mock for its lifetime; the
                // read goes through the raw pointer, forming no reference.
                unsafe { ::core::ptr::addr_of!((*self.as_ptr()).$field).read() }
            }
        }
    )*};
}

mock_getter!(
    Refcounted => rc: Cell<usize>,
    Saturating => rc: Cell<usize>,
    Boxed => payload: u32,
    Dupable => payload: u32,
    Shared => rc: Cell<usize>,
    Shared => payload: u32,
);

// ---------------------------------------------------------------------------
// Buffer element kinds: a plain Rust value gets a real slice, a wrapped C
// object gets handles.
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct elem_st {
    pub tag: u32,
}
ffibox::define_ctype!(Elem, ElemRef, ElemMut, elem_st);

impl ElemRef<'_> {
    fn tag(&self) -> u32 {
        // SAFETY: the handle borrows a live `elem_st`; the read goes through the
        // raw pointer, forming no reference.
        unsafe { core::ptr::addr_of!((*self.as_ptr()).tag).read() }
    }
}

#[test]
fn cvec_of_plain_values_yields_a_real_slice() {
    let _guard = lock(&CVEC_LOCK);
    let v: RecVec<u32> = make_cvec(vec![1u32, 2, 3]);
    // `u32: CElem`, and the buffer is owned exclusively, so `&[u32]` holds.
    assert_eq!(v.as_slice(), &[1, 2, 3]);
    assert_eq!(v.as_slice().iter().sum::<u32>(), 6);
}

#[test]
fn cvec_of_wrapped_objects_yields_handles() {
    let _guard = lock(&CVEC_LOCK);
    // `Elem` implements `CCell`, not `CElem`, so `as_slice()` does not compile
    // for it -- a `&[Elem]` would be a reference covering the C objects. The
    // buffer is reached as handles instead.
    let mut v: RecVec<Elem> = make_cvec(vec![Elem::zeroed(), Elem::zeroed(), Elem::zeroed()]);
    let run = v.as_handles();
    assert_eq!(run.len(), 3);
    assert!(!run.is_empty());
    assert!(run.get(3).is_none());

    // Every element reads back through its own handle.
    assert_eq!(run.get(0).unwrap().tag(), 0);
    assert_eq!(run.iter().map(|e| e.tag()).sum::<u32>(), 0);

    // The shared view's pointer is `*const`: writing takes the exclusive view.
    let _: *const elem_st = run.as_ptr();
    let mut run = v.as_handles_mut();
    // SAFETY: `run` exclusively borrows the live buffer; element 1 is in range.
    unsafe { core::ptr::addr_of_mut!((*run.as_mut_ptr().add(1)).tag).write(7) };
    assert_eq!(v.as_handles().iter().map(|e| e.tag()).sum::<u32>(), 7);
}

impl ElemMut<'_> {
    fn set_tag(&mut self, v: u32) {
        // SAFETY: the exclusive handle borrows a live `elem_st`; the write goes
        // through the raw pointer, forming no reference.
        unsafe { core::ptr::addr_of_mut!((*self.as_mut_ptr()).tag).write(v) }
    }
}

#[test]
fn the_exclusive_run_writes_through_per_element_handles() {
    let _guard = lock(&CVEC_LOCK);
    let mut v: RecVec<Elem> = make_cvec(vec![Elem::zeroed(), Elem::zeroed(), Elem::zeroed()]);
    let mut run = v.as_handles_mut();
    assert_eq!(run.len(), 3);
    assert!(run.get_mut(3).is_none());

    run.get_mut(1).unwrap().set_tag(7);
    assert_eq!(run.get(1).unwrap().tag(), 7);

    // Every item of `iter_mut` addresses a distinct element, so holding them at
    // once is sound -- the same reason `slice::iter_mut` is.
    for (i, mut e) in run.iter_mut().enumerate() {
        e.set_tag(i as u32 + 1);
    }
    assert_eq!(run.as_ref().iter().map(|e| e.tag()).sum::<u32>(), 6);
}

#[test]
fn a_scalar_run_is_read_out_without_forming_a_slice() {
    // What a wrapper reaches for when the run lives inside a C object rather
    // than in a Rust-owned buffer: `CElem` makes every bit pattern valid, but
    // it does not make `&[u32]` sound over memory C writes through a pointer it
    // kept. Elements are copied out one at a time instead.
    let mut buf = [1u32, 2, 3];
    let ptr = core::ptr::NonNull::new(buf.as_mut_ptr()).unwrap();

    // SAFETY: `buf` is live for the rest of the scope and holds 3 `u32`.
    let run: CSlice<'_, u32> = unsafe { CSlice::from_raw_parts(ptr, 3) };
    assert_eq!(run.elem(0), Some(1));
    assert_eq!(run.elem(3), None);
    assert_eq!(run.elems().sum::<u32>(), 6);
    let mut out = [0u32; 3];
    assert!(run.copy_to_slice(&mut out));
    assert_eq!(out, [1, 2, 3]);
    assert!(!run.copy_to_slice(&mut [0u32; 2]));

    // SAFETY: as above, and `run` is dead by here, so this is the only view.
    let mut w: CSliceMut<'_, u32> = unsafe { CSliceMut::from_raw_parts(ptr, 3) };
    assert!(w.set_elem(0, 10));
    assert!(!w.set_elem(3, 10));
    assert_eq!(w.elem(0), Some(10));
    assert!(w.copy_from_slice(&[4, 5, 6]));
    assert!(!w.copy_from_slice(&[4, 5]));
    assert_eq!(w.as_ref().elems().collect::<Vec<_>>(), vec![4, 5, 6]);
    assert_eq!(buf, [4, 5, 6]);
}
