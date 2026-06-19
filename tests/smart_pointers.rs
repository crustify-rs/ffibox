//! Integration tests for crustify's smart pointers and macros.

#![allow(missing_docs)]
//!
//! Tests use mock "C" types backed by [`AtomicUsize`] counters to verify
//! that:
//!
//! - `CBox` calls `c_drop` unconditionally on drop, and `c_clone` on clone —
//!   over both a refcounted pointee (down-ref / `up_ref`) and a sole-owner one
//!   (`*_free` / `*_dup`); `c_clone` spans both mechanisms
//! - `CVec` calls the strategy's `cleanup` with the correct byte length
//! - `into_raw` suppresses cleanup
//! - layout invariants hold (size of `CBox<T>` == size of `*mut T`, etc.)

use core::cell::Cell;
use core::ffi::c_void;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};

use crustify_prim::{
    impl_dropped, CBox, CCell, CCloned, CDropped, CDroppedUninit, CLenDropped, CVal, CValGuard,
    CValued, CVec, CVoidBox,
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
static VALUED_LOCK: Mutex<()> = Mutex::new(());
static GUARD_LOCK: Mutex<()> = Mutex::new(());
static CVOIDBOX_LOCK: Mutex<()> = Mutex::new(());

fn lock(m: &'static Mutex<()>) -> MutexGuard<'static, ()> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---------------------------------------------------------------------------
// Refcounted mock — a CBox whose c_drop is the down-ref and c_clone the up_ref
// ---------------------------------------------------------------------------

static REFCOUNTED_UP_REF_CALLS: AtomicUsize = AtomicUsize::new(0);
static REFCOUNTED_DOWN_REF_CALLS: AtomicUsize = AtomicUsize::new(0);
static REFCOUNTED_FREED: AtomicUsize = AtomicUsize::new(0);
static REFCOUNTED_UNINIT_FREED: AtomicUsize = AtomicUsize::new(0);

#[repr(C)]
struct Refcounted {
    /// Real C refcount field. `Cell` because we mutate through `&self`
    /// (interior mutability) to mirror how C code mutates through shared
    /// pointers.
    rc: Cell<usize>,
}

// SAFETY: `c_clone` increments the refcount via interior mutability;
// `c_unref` decrements it and reclaims the Box when it reaches zero.
// This mirrors the C refcount contract.
unsafe impl CCloned for Refcounted {
    unsafe fn c_clone(obj: NonNull<Self>) -> Option<NonNull<Self>> {
        REFCOUNTED_UP_REF_CALLS.fetch_add(1, Ordering::SeqCst);
        // SAFETY: invariants of CCloned::c_clone hold per test setup.
        let this = unsafe { obj.as_ref() };
        this.rc.set(this.rc.get() + 1);
        // Refcount bump: the SAME pointer, now owing one more c_drop.
        Some(obj)
    }
}

unsafe impl CDropped for Refcounted {
    unsafe fn c_drop(obj: NonNull<Self>) {
        REFCOUNTED_DOWN_REF_CALLS.fetch_add(1, Ordering::SeqCst);
        // SAFETY: invariants of CDropped::c_drop hold per test setup.
        let this = unsafe { obj.as_ref() };
        let new = this.rc.get() - 1;
        this.rc.set(new);
        if new == 0 {
            REFCOUNTED_FREED.fetch_add(1, Ordering::SeqCst);
            // SAFETY: refcount dropped to zero — reclaim the box we
            // leaked in `make_refcounted`.
            drop(unsafe { Box::from_raw(obj.as_ptr()) });
        }
    }
}

// Uninit-phase storage free: a CBoxUninit dropped before `assume_init`
// (construction failure) reclaims the raw allocation WITHOUT running the
// down-ref `c_drop` (the refcount isn't established yet). For this Box-backed
// fixture the storage is the whole object.
// SAFETY: reclaims the leaked Box exactly once; no field teardown.
unsafe impl CDroppedUninit for Refcounted {
    unsafe fn c_drop_uninit(obj: NonNull<Self>) {
        REFCOUNTED_UNINIT_FREED.fetch_add(1, Ordering::SeqCst);
        drop(unsafe { Box::from_raw(obj.as_ptr()) });
    }
}

fn make_refcounted() -> CBox<Refcounted> {
    let leaked = Box::into_raw(Box::new(Refcounted { rc: Cell::new(1) }));
    // SAFETY: `leaked` is non-null and represents one outstanding refcount.
    unsafe { CBox::from_raw(leaked) }.unwrap()
}

#[test]
fn refcounted_cbox_clone_calls_up_ref() {
    let _guard = lock(&REFCOUNTED_LOCK);
    REFCOUNTED_UP_REF_CALLS.store(0, Ordering::SeqCst);
    REFCOUNTED_DOWN_REF_CALLS.store(0, Ordering::SeqCst);
    REFCOUNTED_FREED.store(0, Ordering::SeqCst);

    let a = make_refcounted();
    assert_eq!(a.rc.get(), 1);

    let b = a.clone();
    assert_eq!(REFCOUNTED_UP_REF_CALLS.load(Ordering::SeqCst), 1);
    assert_eq!(a.rc.get(), 2);
    assert_eq!(b.rc.get(), 2);

    let c = b.clone();
    assert_eq!(REFCOUNTED_UP_REF_CALLS.load(Ordering::SeqCst), 2);
    assert_eq!(c.rc.get(), 3);

    // Pointers should all be equal (same object, just more refs).
    assert_eq!(a.as_ptr(), b.as_ptr());
    assert_eq!(b.as_ptr(), c.as_ptr());

    drop(a);
    drop(b);
    assert_eq!(REFCOUNTED_FREED.load(Ordering::SeqCst), 0);
    drop(c);
    assert_eq!(REFCOUNTED_DOWN_REF_CALLS.load(Ordering::SeqCst), 3);
    assert_eq!(REFCOUNTED_FREED.load(Ordering::SeqCst), 1);
}

#[test]
fn refcounted_cbox_into_raw_preserves_refcount() {
    let _guard = lock(&REFCOUNTED_LOCK);
    REFCOUNTED_DOWN_REF_CALLS.store(0, Ordering::SeqCst);
    REFCOUNTED_FREED.store(0, Ordering::SeqCst);

    let a = make_refcounted();
    let raw = a.into_raw();
    assert_eq!(REFCOUNTED_DOWN_REF_CALLS.load(Ordering::SeqCst), 0);

    // SAFETY: `raw` came from `into_raw` and represents one refcount.
    let restored = unsafe { CBox::<Refcounted>::from_raw(raw) }.unwrap();
    drop(restored);
    assert_eq!(REFCOUNTED_DOWN_REF_CALLS.load(Ordering::SeqCst), 1);
    assert_eq!(REFCOUNTED_FREED.load(Ordering::SeqCst), 1);
}

#[test]
fn refcounted_cbox_from_raw_null_returns_none() {
    // SAFETY: passing null is explicitly the documented `None` case.
    let v: Option<CBox<Refcounted>> = unsafe { CBox::from_raw(core::ptr::null_mut()) };
    assert!(v.is_none());
}

// ---------------------------------------------------------------------------
// try_clone — fallible clone that mirrors C's recoverable-error semantics
// ---------------------------------------------------------------------------

/// A refcounted mock that simulates refcount overflow: `c_clone` returns
/// `None` once the counter reaches `MAX_RC`, replicating what a real C
/// `*_up_ref` reports on integer overflow.
const SATURATING_MAX: usize = 3;

#[repr(C)]
struct Saturating {
    rc: Cell<usize>,
}

// SAFETY: `c_clone` returns false on overflow (simulating INT_MAX exceeded),
// `c_unref` reclaims the Box on zero.
unsafe impl CCloned for Saturating {
    unsafe fn c_clone(obj: NonNull<Self>) -> Option<NonNull<Self>> {
        // SAFETY: caller upholds CCloned::c_clone contract.
        let this = unsafe { obj.as_ref() };
        if this.rc.get() >= SATURATING_MAX {
            return None; // simulate refcount overflow
        }
        this.rc.set(this.rc.get() + 1);
        Some(obj)
    }
}

unsafe impl CDropped for Saturating {
    unsafe fn c_drop(obj: NonNull<Self>) {
        // SAFETY: caller upholds CDropped::c_drop contract.
        let this = unsafe { obj.as_ref() };
        let new = this.rc.get() - 1;
        this.rc.set(new);
        if new == 0 {
            // SAFETY: refcount zero — reclaim the Box.
            drop(unsafe { Box::from_raw(obj.as_ptr()) });
        }
    }
}

fn make_saturating() -> CBox<Saturating> {
    let leaked = Box::into_raw(Box::new(Saturating { rc: Cell::new(1) }));
    // SAFETY: non-null, represents one refcount.
    unsafe { CBox::from_raw(leaked) }.unwrap()
}

#[test]
fn try_clone_succeeds_below_overflow() {
    let a = make_saturating();
    assert_eq!(a.rc.get(), 1);

    let b = a.try_clone().expect("try_clone should succeed below MAX");
    assert_eq!(a.rc.get(), 2);
    assert_eq!(a.as_ptr(), b.as_ptr()); // same object

    let c = a.try_clone().expect("try_clone should succeed at MAX-1");
    assert_eq!(a.rc.get(), 3);

    drop(b);
    drop(c);
    assert_eq!(a.rc.get(), 1);
}

#[test]
fn try_clone_returns_none_on_overflow() {
    let a = make_saturating();

    // Bump to the limit.
    let _b = a.try_clone().unwrap(); // rc = 2
    let _c = a.try_clone().unwrap(); // rc = 3 = SATURATING_MAX

    // Next try_clone must return None — up_ref reports overflow.
    let overflow = a.try_clone();
    assert!(
        overflow.is_none(),
        "try_clone must return None when up_ref signals overflow"
    );

    // Object is still live and its refcount is unchanged.
    assert_eq!(a.rc.get(), SATURATING_MAX);
}

#[test]
fn try_clone_none_does_not_create_phantom_handle() {
    let a = make_saturating();
    let _b = a.try_clone().unwrap(); // rc = 2
    let _c = a.try_clone().unwrap(); // rc = 3 = SATURATING_MAX

    let phantom = a.try_clone(); // None — no new handle
    assert!(phantom.is_none());

    // Drop order: _c, _b, a → each decrements once → frees at rc=0.
    // If try_clone had created a phantom handle the Drop would decrement
    // one extra time, going below zero and corrupting the counter.
    drop(_c);
    assert_eq!(a.rc.get(), 2);
    drop(_b);
    assert_eq!(a.rc.get(), 1);
    // `a` drops last — object is freed.
}

// ---------------------------------------------------------------------------
// Unique-owned mock — exercised by CBox
// ---------------------------------------------------------------------------

static BOXED_FREE_CALLS: AtomicUsize = AtomicUsize::new(0);
static BOXED_UNINIT_FREE_CALLS: AtomicUsize = AtomicUsize::new(0);

#[repr(C)]
struct Boxed {
    payload: u32,
    /// Internal teardown gate. `CBox` always calls `c_drop`; the decision to
    /// actually reclaim folds INTO `c_drop` (the recommended pattern).
    /// `false` leaves the storage for the caller.
    should_free: bool,
}

// SAFETY: `c_drop` is always invoked by `CBox::drop`; it reclaims the backing
// Box exactly once when its internal `should_free` gate is set.
unsafe impl CDropped for Boxed {
    unsafe fn c_drop(obj: NonNull<Self>) {
        BOXED_FREE_CALLS.fetch_add(1, Ordering::SeqCst);
        // SAFETY: invariants of CDropped hold per test setup. The gate lives
        // here now: only reclaim the leaked Box when the object asks for it.
        if unsafe { obj.as_ref() }.should_free {
            drop(unsafe { Box::from_raw(obj.as_ptr()) });
        }
    }
}

// Uninit-phase storage free: a CBoxUninit dropped before `assume_init` reclaims
// the raw allocation without `c_drop`'s field teardown (here the fields are
// inline, so storage-free is the whole reclaim).
// SAFETY: reclaims the leaked Box exactly once; no field teardown.
unsafe impl CDroppedUninit for Boxed {
    unsafe fn c_drop_uninit(obj: NonNull<Self>) {
        BOXED_UNINIT_FREE_CALLS.fetch_add(1, Ordering::SeqCst);
        drop(unsafe { Box::from_raw(obj.as_ptr()) });
    }
}

fn make_boxed(should_free: bool) -> CBox<Boxed> {
    let leaked = Box::into_raw(Box::new(Boxed {
        payload: 42,
        should_free,
    }));
    // SAFETY: `leaked` is non-null and uniquely owned.
    unsafe { CBox::from_raw(leaked) }.unwrap()
}

#[test]
fn cbox_drop_calls_c_drop() {
    let _guard = lock(&BOXED_LOCK);
    BOXED_FREE_CALLS.store(0, Ordering::SeqCst);

    let b = make_boxed(true);
    assert_eq!(b.payload, 42);
    drop(b);
    assert_eq!(BOXED_FREE_CALLS.load(Ordering::SeqCst), 1);
}

#[test]
fn cbox_always_calls_c_drop_gate_folds_internally() {
    let _guard = lock(&BOXED_LOCK);
    BOXED_FREE_CALLS.store(0, Ordering::SeqCst);

    // `should_free = false`: CBox still calls c_drop unconditionally, but the
    // gate inside c_drop declines to reclaim, leaving the storage to us.
    let b = make_boxed(false);
    let raw = b.as_ptr();
    drop(b);
    assert_eq!(
        BOXED_FREE_CALLS.load(Ordering::SeqCst),
        1,
        "CBox::drop must call c_drop unconditionally; the skip gate lives inside c_drop"
    );
    // Reclaim the leaked box (c_drop's internal gate declined to).
    // SAFETY: `raw` was the only outstanding pointer and c_drop did not
    // free it (should_free = false).
    drop(unsafe { Box::from_raw(raw) });
}

#[test]
fn cbox_into_raw_suppresses_drop() {
    let _guard = lock(&BOXED_LOCK);
    BOXED_FREE_CALLS.store(0, Ordering::SeqCst);

    let b = make_boxed(true);
    let raw = b.into_raw();
    assert_eq!(BOXED_FREE_CALLS.load(Ordering::SeqCst), 0);

    // SAFETY: `raw` came from `into_raw` and is still uniquely owned.
    let restored = unsafe { CBox::<Boxed>::from_raw(raw) }.unwrap();
    drop(restored);
    assert_eq!(BOXED_FREE_CALLS.load(Ordering::SeqCst), 1);
}

// ---------------------------------------------------------------------------
// CCloned — opt-in deep clone on CBox
// ---------------------------------------------------------------------------

static DUPABLE_FREE_CALLS: AtomicUsize = AtomicUsize::new(0);
static DUPABLE_DUP_CALLS: AtomicUsize = AtomicUsize::new(0);
/// When non-zero, the next `c_clone` returns `None` and decrements this
/// counter — used to drive the fallible path without races between tests
/// (each test resets it explicitly).
static DUPABLE_DUP_FAILURES: AtomicUsize = AtomicUsize::new(0);

#[repr(C)]
struct Dupable {
    payload: u32,
}

// SAFETY: `c_drop` reclaims the Box backing the object exactly once.
unsafe impl CDropped for Dupable {
    unsafe fn c_drop(obj: NonNull<Self>) {
        DUPABLE_FREE_CALLS.fetch_add(1, Ordering::SeqCst);
        // SAFETY: invariants of CDropped hold per test setup; reclaim the
        // leaked Box.
        drop(unsafe { Box::from_raw(obj.as_ptr()) });
    }
}

// SAFETY: `c_clone` allocates a brand-new owned Box (independent of `obj`)
// and returns `None` when the test has armed the failure counter. The
// returned pointer is releasable through the same `c_drop` impl as the
// original — both copies share the destructor contract.
unsafe impl CCloned for Dupable {
    unsafe fn c_clone(obj: NonNull<Self>) -> Option<NonNull<Self>> {
        DUPABLE_DUP_CALLS.fetch_add(1, Ordering::SeqCst);
        if DUPABLE_DUP_FAILURES.load(Ordering::SeqCst) > 0 {
            DUPABLE_DUP_FAILURES.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        // SAFETY: `obj` is a live `Dupable` per the trait contract.
        let payload = unsafe { obj.as_ref() }.payload;
        let leaked = Box::into_raw(Box::new(Dupable { payload }));
        NonNull::new(leaked)
    }
}

fn make_dupable(payload: u32) -> CBox<Dupable> {
    let leaked = Box::into_raw(Box::new(Dupable { payload }));
    // SAFETY: `leaked` is non-null and uniquely owned.
    unsafe { CBox::from_raw(leaked) }.unwrap()
}

#[test]
fn cbox_clone_invokes_c_clone_and_produces_independent_handle() {
    let _guard = lock(&DUPABLE_LOCK);
    DUPABLE_FREE_CALLS.store(0, Ordering::SeqCst);
    DUPABLE_DUP_CALLS.store(0, Ordering::SeqCst);
    DUPABLE_DUP_FAILURES.store(0, Ordering::SeqCst);

    let a = make_dupable(123);
    let b = a.clone();

    assert_eq!(DUPABLE_DUP_CALLS.load(Ordering::SeqCst), 1);
    // Distinct allocations — deep clone, not refcount bump.
    assert_ne!(a.as_ptr(), b.as_ptr());
    assert_eq!(a.payload, 123);
    assert_eq!(b.payload, 123);

    // Each handle owns its own allocation: dropping both calls c_drop twice.
    drop(a);
    drop(b);
    assert_eq!(DUPABLE_FREE_CALLS.load(Ordering::SeqCst), 2);
}

#[test]
fn cbox_try_clone_succeeds_returns_some() {
    let _guard = lock(&DUPABLE_LOCK);
    DUPABLE_FREE_CALLS.store(0, Ordering::SeqCst);
    DUPABLE_DUP_CALLS.store(0, Ordering::SeqCst);
    DUPABLE_DUP_FAILURES.store(0, Ordering::SeqCst);

    let a = make_dupable(7);
    let b = a.try_clone().expect("c_clone success path must yield Some");
    assert_eq!(b.payload, 7);
    assert_ne!(a.as_ptr(), b.as_ptr());

    drop(a);
    drop(b);
    assert_eq!(DUPABLE_FREE_CALLS.load(Ordering::SeqCst), 2);
}

#[test]
fn cbox_try_clone_returns_none_on_c_clone_failure() {
    let _guard = lock(&DUPABLE_LOCK);
    DUPABLE_FREE_CALLS.store(0, Ordering::SeqCst);
    DUPABLE_DUP_CALLS.store(0, Ordering::SeqCst);
    DUPABLE_DUP_FAILURES.store(1, Ordering::SeqCst);

    let a = make_dupable(99);
    let attempt = a.try_clone();
    assert!(
        attempt.is_none(),
        "try_clone must propagate c_clone failure"
    );
    // Original handle is still live and untouched.
    assert_eq!(a.payload, 99);

    drop(a);
    // Only the original is freed — failed clone did not produce a handle.
    assert_eq!(DUPABLE_FREE_CALLS.load(Ordering::SeqCst), 1);
}

// ---------------------------------------------------------------------------
// CVec — strategy-based cleanup
// ---------------------------------------------------------------------------

static CVEC_CLEANUP_CALLS: AtomicUsize = AtomicUsize::new(0);
static CVEC_LAST_BYTE_LEN: AtomicUsize = AtomicUsize::new(usize::MAX);

/// Cleanup strategy that records each call so tests can verify the byte
/// length passed in. Reclaims the underlying allocation via `Box::from_raw`.
struct RecordingFree;

// SAFETY: tests always construct `CVec<_, RecordingFree>` via
// `make_cvec`, which leaks a `Box<[u8]>` whose layout matches the
// `(ptr, byte_len)` pair passed to `cleanup` here.
unsafe impl CLenDropped for RecordingFree {
    unsafe fn c_drop_len(ptr: *mut u8, byte_len: usize) {
        CVEC_CLEANUP_CALLS.fetch_add(1, Ordering::SeqCst);
        CVEC_LAST_BYTE_LEN.store(byte_len, Ordering::SeqCst);
        // SAFETY: tests construct CVec from `Box::into_raw` of a
        // boxed slice of `byte_len` bytes.
        let slice = unsafe { core::slice::from_raw_parts_mut(ptr, byte_len) };
        // SAFETY: same — reclaim the boxed slice we leaked.
        drop(unsafe { Box::from_raw(slice as *mut [u8]) });
    }
}

fn make_cvec<T>(elems: Vec<T>) -> CVec<T, RecordingFree> {
    let boxed = elems.into_boxed_slice();
    let count = boxed.len();
    let ptr = Box::into_raw(boxed) as *mut T;
    // SAFETY: `ptr` is non-null, `count` matches the allocation.
    unsafe { CVec::from_raw_parts(ptr, count) }.unwrap()
}

#[test]
fn cvec_basic_slice_view() {
    let v: CVec<u32, RecordingFree> = make_cvec(vec![1u32, 2, 3, 4]);
    assert_eq!(v.count(), 4);
    assert_eq!(v.byte_len(), 16);
    assert!(!v.is_empty());
    assert_eq!(v.as_slice(), &[1, 2, 3, 4]);
}

#[test]
fn cvec_mutable_slice_view() {
    let mut v: CVec<u8, RecordingFree> = make_cvec(vec![0u8, 0, 0]);
    v.as_mut_slice().copy_from_slice(&[10, 20, 30]);
    assert_eq!(v.as_slice(), &[10, 20, 30]);
}

#[test]
fn cvec_drop_calls_cleanup_with_correct_byte_len() {
    let _guard = lock(&CVEC_LOCK);
    CVEC_CLEANUP_CALLS.store(0, Ordering::SeqCst);
    CVEC_LAST_BYTE_LEN.store(usize::MAX, Ordering::SeqCst);

    let v: CVec<u32, RecordingFree> = make_cvec(vec![0u32; 8]);
    let expected_bytes = 8 * core::mem::size_of::<u32>();
    drop(v);

    assert_eq!(CVEC_CLEANUP_CALLS.load(Ordering::SeqCst), 1);
    assert_eq!(CVEC_LAST_BYTE_LEN.load(Ordering::SeqCst), expected_bytes);
}

#[test]
fn cvec_into_raw_parts_suppresses_cleanup() {
    let _guard = lock(&CVEC_LOCK);
    CVEC_CLEANUP_CALLS.store(0, Ordering::SeqCst);

    let v: CVec<u8, RecordingFree> = make_cvec(vec![1u8, 2, 3]);
    let (ptr, count) = v.into_raw_parts();
    assert_eq!(count, 3);
    assert_eq!(CVEC_CLEANUP_CALLS.load(Ordering::SeqCst), 0);

    // SAFETY: `ptr` was just returned from `into_raw_parts`; reclaiming
    // it through a new `CVec` is sound. Drop will trigger cleanup.
    let restored: CVec<u8, RecordingFree> = unsafe { CVec::from_raw_parts(ptr, count) }.unwrap();
    drop(restored);
    assert_eq!(CVEC_CLEANUP_CALLS.load(Ordering::SeqCst), 1);
}

// ---------------------------------------------------------------------------
// Layout invariants
// ---------------------------------------------------------------------------

#[test]
fn refcounted_cbox_is_pointer_sized() {
    assert_eq!(
        core::mem::size_of::<CBox<Refcounted>>(),
        core::mem::size_of::<*mut Refcounted>(),
    );
    // NonNull niche: Option<CBox<T>> stays pointer-sized.
    assert_eq!(
        core::mem::size_of::<Option<CBox<Refcounted>>>(),
        core::mem::size_of::<*mut Refcounted>(),
    );
}

#[test]
fn cbox_is_pointer_sized() {
    assert_eq!(
        core::mem::size_of::<CBox<Boxed>>(),
        core::mem::size_of::<*mut Boxed>(),
    );
    assert_eq!(
        core::mem::size_of::<Option<CBox<Boxed>>>(),
        core::mem::size_of::<*mut Boxed>(),
    );
}

#[test]
fn cvec_is_ptr_plus_usize() {
    assert_eq!(
        core::mem::size_of::<CVec<u8, RecordingFree>>(),
        core::mem::size_of::<*mut u8>() + core::mem::size_of::<usize>(),
    );
}

// ---------------------------------------------------------------------------
// CBoxUninit — construction-phase handle for owned objects (storage-free Drop; CCell Deref)
// ---------------------------------------------------------------------------

use core::mem::MaybeUninit;
use crustify_prim::CBoxUninit;

#[test]
fn cboxuninit_from_raw_null_returns_none() {
    let v: Option<CBoxUninit<Boxed>> =
        unsafe { CBoxUninit::from_raw_uninit(core::ptr::null_mut()) };
    assert!(v.is_none());
}

#[test]
fn cboxuninit_is_pointer_sized() {
    assert_eq!(
        core::mem::size_of::<CBoxUninit<Boxed>>(),
        core::mem::size_of::<*mut Boxed>(),
    );
    assert_eq!(
        core::mem::size_of::<Option<CBoxUninit<Boxed>>>(),
        core::mem::size_of::<*mut Boxed>(),
    );
}

#[test]
fn cboxuninit_assume_init_promotes_and_c_drop_runs_once() {
    let _guard = lock(&BOXED_LOCK);
    BOXED_FREE_CALLS.store(0, Ordering::SeqCst);

    // Simulate a C allocator returning fresh uninit memory.
    let alloc: *mut MaybeUninit<Boxed> = Box::into_raw(Box::new(MaybeUninit::<Boxed>::uninit()));

    let mut uninit = unsafe { CBoxUninit::from_raw_uninit(alloc) }.unwrap();

    // Initialise via raw pointer projection.
    unsafe {
        core::ptr::write(
            uninit.as_mut_ptr(),
            Boxed {
                payload: 7,
                should_free: true,
            },
        );
    }

    let boxed: CBox<Boxed> = unsafe { uninit.assume_init() };
    assert_eq!(boxed.payload, 7);
    assert_eq!(BOXED_FREE_CALLS.load(Ordering::SeqCst), 0);

    drop(boxed);
    assert_eq!(
        BOXED_FREE_CALLS.load(Ordering::SeqCst),
        1,
        "c_drop must run exactly once after assume_init + drop"
    );
}

#[test]
fn cboxuninit_drop_does_not_call_c_drop() {
    let _guard = lock(&BOXED_LOCK);
    // Dropping the uninit handle without consuming it reclaims the storage via
    // `c_drop_uninit` (RAII on construction failure) but MUST NOT call `c_drop`
    // — the slot is not fully initialised, so its field teardown is unsound.
    BOXED_FREE_CALLS.store(0, Ordering::SeqCst);
    BOXED_UNINIT_FREE_CALLS.store(0, Ordering::SeqCst);

    let alloc: *mut MaybeUninit<Boxed> = Box::into_raw(Box::new(MaybeUninit::<Boxed>::uninit()));

    {
        let uninit = unsafe { CBoxUninit::from_raw_uninit(alloc) }.unwrap();
        drop(uninit);
        // `Drop` reclaimed `alloc` via `c_drop_uninit`; freeing it again would
        // double-free.
    }

    assert_eq!(
        BOXED_FREE_CALLS.load(Ordering::SeqCst),
        0,
        "CBoxUninit::drop must NOT call c_drop (slot not fully initialised)"
    );
    assert_eq!(
        BOXED_UNINIT_FREE_CALLS.load(Ordering::SeqCst),
        1,
        "CBoxUninit::drop must reclaim storage via c_drop_uninit exactly once"
    );
}

#[test]
fn cboxuninit_into_raw_uninit_releases_pointer() {
    let _guard = lock(&BOXED_LOCK);
    BOXED_FREE_CALLS.store(0, Ordering::SeqCst);

    let alloc: *mut MaybeUninit<Boxed> = Box::into_raw(Box::new(MaybeUninit::<Boxed>::uninit()));
    let uninit = unsafe { CBoxUninit::from_raw_uninit(alloc) }.unwrap();
    let raw = uninit.into_raw_uninit();
    assert_eq!(
        raw, alloc,
        "into_raw_uninit must return the original pointer"
    );
    assert_eq!(BOXED_FREE_CALLS.load(Ordering::SeqCst), 0);

    // Caller is responsible for reclaiming.
    drop(unsafe { Box::from_raw(raw) });
}

// ---------------------------------------------------------------------------
// CValued — by-value owned resource, exercised by CVal
// ---------------------------------------------------------------------------

static VALUED_DROP_CALLS: AtomicUsize = AtomicUsize::new(0);

/// A by-value C type: Rust owns the header inline; `c_dispose` disposes an owned
/// resource (here, just counted) WITHOUT freeing the header.
#[repr(C)]
struct Valued {
    payload: u32,
    /// Internal disposal gate. `CVal` always calls `c_dispose`; whether the
    /// owned resource is actually reclaimed folds INTO `c_dispose` (the
    /// recommended pattern).
    should_dispose: bool,
}

// SAFETY: `c_dispose` disposes the (counted) owned resource exactly once and
// never frees the header — the header is the caller's inline storage.
unsafe impl CValued for Valued {
    unsafe fn c_dispose(this: NonNull<Self>) {
        // CVal always calls this; the gate folds in. No header free: the value
        // lives inline in the CVal, owned by Rust.
        if unsafe { this.as_ref() }.should_dispose {
            VALUED_DROP_CALLS.fetch_add(1, Ordering::SeqCst);
        }
    }
}

#[test]
fn cvalue_drop_calls_c_dispose_once() {
    let _guard = lock(&VALUED_LOCK);
    VALUED_DROP_CALLS.store(0, Ordering::SeqCst);

    let v = CVal::new(Valued {
        payload: 7,
        should_dispose: true,
    });
    // Deref reaches the inner value's fields.
    assert_eq!(v.payload, 7);
    drop(v);
    assert_eq!(VALUED_DROP_CALLS.load(Ordering::SeqCst), 1);
}

#[test]
fn cvalue_c_dispose_gate_folds_internally() {
    let _guard = lock(&VALUED_LOCK);
    VALUED_DROP_CALLS.store(0, Ordering::SeqCst);

    let v = CVal::new(Valued {
        payload: 0,
        should_dispose: false,
    });
    drop(v);
    assert_eq!(
        VALUED_DROP_CALLS.load(Ordering::SeqCst),
        0,
        "CVal always calls c_dispose; the folded gate declined the counted work"
    );
}

// Dedicated CValued fixture for the CValGuard test, with its OWN drop counter:
// the shared `VALUED_DROP_CALLS` races against the `cvalue_*` tests under
// parallel execution, so the guard test must not touch it.
static GUARD_DROP_CALLS: AtomicUsize = AtomicUsize::new(0);

#[repr(C)]
struct GuardValued {
    payload: u32,
}

// SAFETY: `c_dispose` disposes the (counted) owned resource and never frees the
// header — the value is borrowed in place by the guard.
unsafe impl CValued for GuardValued {
    unsafe fn c_dispose(_this: NonNull<Self>) {
        GUARD_DROP_CALLS.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn cvalguard_disposes_in_place_unless_dismissed() {
    let _guard = lock(&GUARD_LOCK);
    // An embedded value the guard BORROWS (never owns / moves). Its address is
    // pinned; the guard only disposes its fields in place.
    let mut embedded = GuardValued { payload: 9 };
    let addr_before = &embedded as *const GuardValued;

    // dismiss → mem::forget the guard → no c_dispose, and `embedded` never moved.
    GUARD_DROP_CALLS.store(0, Ordering::SeqCst);
    {
        // SAFETY: `embedded` is live and initialised; we accept in-place dispose.
        let g = unsafe { CValGuard::new(&mut embedded) };
        assert_eq!(g.payload, 9); // Deref reaches the borrowed value
        g.dismiss();
    }
    assert_eq!(
        GUARD_DROP_CALLS.load(Ordering::SeqCst),
        0,
        "dismiss must suppress in-place c_dispose",
    );
    assert_eq!(
        &embedded as *const GuardValued, addr_before,
        "dismiss must not relocate the borrowed value",
    );

    // armed path → guard drops → c_dispose runs in place, value still not moved.
    GUARD_DROP_CALLS.store(0, Ordering::SeqCst);
    {
        // SAFETY: `embedded` is live and initialised; in-place dispose accepted.
        let _g = unsafe { CValGuard::new(&mut embedded) };
    }
    assert_eq!(GUARD_DROP_CALLS.load(Ordering::SeqCst), 1);
    assert_eq!(&embedded as *const GuardValued, addr_before);
}

// NOTE: `CVal` is deliberately `Deref`-only (no `DerefMut`). A plain
// `CVal<T>` over a non-`UnsafeCell` `T` has no sound `&self` mutation path —
// mutating through a shared deref would be UB. Interior mutation is a
// wrapper/`CCell` concern, exercised by the `define_type!` wrapper tests, not
// `CVal` itself. (The former `cvalue_deref_mut_reaches_inner` test was removed
// when `CVal::DerefMut` was dropped.)

// ---------------------------------------------------------------------------
// CVoidBox<D> — type-erased owned `void *` with a static deleter class
// ---------------------------------------------------------------------------
//
// Unlike `CBox<T>` (typed, dereferenceable), `CVoidBox<D>` keeps the pointee
// erased throughout: only the deleter class `D` (a ZST `CDropped` marker) is
// known. The bytes behind the `void *` are never read as a Rust type — they
// are merely owned and freed. These tests use a Rust-allocated blob standing
// in for a C allocation, freed through a C-style free function registered on
// the marker via `impl_dropped!`.

static COWN_FREE_CALLS: AtomicUsize = AtomicUsize::new(0);
static COWN_FREED_PTR: core::sync::atomic::AtomicPtr<c_void> =
    core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

// The opaque allocation hiding behind the `void *`. `CVoidBox` never names it;
// only the test's free routine does, to reclaim and drop it.
#[repr(C)]
struct ErasedBlob {
    payload: u32,
}

// A C-style destructor (`unsafe extern "C" fn(*mut c_void)`), mirroring a
// real free routine such as libgit2's `git__free`.
unsafe extern "C" fn test_blob_free(ptr: *mut c_void) {
    COWN_FREE_CALLS.fetch_add(1, Ordering::SeqCst);
    COWN_FREED_PTR.store(ptr, Ordering::SeqCst);
    // Reclaim the Rust-allocated blob standing in for the C allocation.
    drop(unsafe { Box::from_raw(ptr.cast::<ErasedBlob>()) });
}

// ZST deleter-class marker: names *how to free*, not *what*. Registered with
// `impl_dropped!` so `CVoidBox<TestBlobFree>` dispatches drops to `test_blob_free`.
struct TestBlobFree;
impl_dropped!(TestBlobFree, c_void, test_blob_free);

// Clarity alias — how a port would name an erased owned blob freed by this
// class (cf. `type GitOwnedBuf = CVoidBox<GitMallocFree>;`).
type OwnedBlob = CVoidBox<TestBlobFree>;

// Leak an `ErasedBlob` and hand its address out as an opaque `void *`.
fn make_owned_blob(payload: u32) -> (OwnedBlob, *mut c_void) {
    let raw = Box::into_raw(Box::new(ErasedBlob { payload })).cast::<c_void>();
    // SAFETY: `raw` is a fresh, uniquely-owned allocation; `TestBlobFree`'s
    // `c_drop` (→ `test_blob_free`) is its correct destructor.
    (unsafe { CVoidBox::from_raw(raw) }.unwrap(), raw)
}

#[test]
fn cown_drop_frees_once_via_deleter_class() {
    let _guard = lock(&CVOIDBOX_LOCK);
    COWN_FREE_CALLS.store(0, Ordering::SeqCst);
    COWN_FREED_PTR.store(core::ptr::null_mut(), Ordering::SeqCst);

    let (own, raw) = make_owned_blob(0xABCD);
    assert_eq!(COWN_FREE_CALLS.load(Ordering::SeqCst), 0);

    drop(own);

    assert_eq!(
        COWN_FREE_CALLS.load(Ordering::SeqCst),
        1,
        "Drop must invoke the deleter class exactly once"
    );
    assert_eq!(
        COWN_FREED_PTR.load(Ordering::SeqCst),
        raw,
        "the deleter must receive the original erased address (no header, no cast drift)"
    );
}

#[test]
fn cown_from_null_is_none() {
    // SAFETY: null is the documented `None` case.
    assert!(unsafe { CVoidBox::<TestBlobFree>::from_raw(core::ptr::null_mut()) }.is_none());
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
        "into_raw must not run the deleter"
    );

    // Reclaim from the slot — the deleter class rides along in the type, so
    // no extra data is threaded through C.
    // SAFETY: `foreign` came from `into_raw` on the same `CVoidBox<TestBlobFree>`
    // and has not been consumed since.
    let own = unsafe { CVoidBox::<TestBlobFree>::from_raw(foreign) }.unwrap();
    assert_eq!(COWN_FREE_CALLS.load(Ordering::SeqCst), 0);

    drop(own);
    assert_eq!(
        COWN_FREE_CALLS.load(Ordering::SeqCst),
        1,
        "exactly one free after the full round-trip"
    );
}

#[test]
fn cown_is_repr_transparent_voidptr() {
    use core::mem::{align_of, size_of};

    // `#[repr(transparent)]` over `NonNull<c_void>`: same layout as a raw
    // `void *`, and `Option<CVoidBox<_>>` is the null-niche `void *`.
    assert_eq!(size_of::<OwnedBlob>(), size_of::<*mut c_void>());
    assert_eq!(align_of::<OwnedBlob>(), align_of::<*mut c_void>());
    assert_eq!(size_of::<Option<OwnedBlob>>(), size_of::<*mut c_void>());

    // as_ptr / into_raw alias the original allocation exactly.
    let (own, raw) = make_owned_blob(1);
    assert_eq!(own.as_ptr(), raw);
    drop(own);
}

// The mocks above play both wrapper and C type. `CBox`/`CBox`/`CVal` now bound
// `T: CCell`; each mock is layout-identical to `CType<Self>`, so it stands in as
// its own `C`. The `CCell` seam is never invoked (the pointers use their own
// `as_ptr`); these impls only satisfy the marker bound.
// SAFETY: `#[repr(C)]` mock is layout-compatible with `CType<Self>`.
unsafe impl CCell for Refcounted {
    type C = Refcounted;
}
// SAFETY: as above.
unsafe impl CCell for Saturating {
    type C = Saturating;
}
// SAFETY: as above.
unsafe impl CCell for Boxed {
    type C = Boxed;
}
// SAFETY: as above.
unsafe impl CCell for Dupable {
    type C = Dupable;
}
// SAFETY: as above.
unsafe impl CCell for Valued {
    type C = Valued;
}
// SAFETY: as above.
unsafe impl CCell for GuardValued {
    type C = GuardValued;
}
