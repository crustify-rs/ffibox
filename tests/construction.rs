//! Integration tests for the construction policies: [`CNew`], [`CAlloc`],
//! [`CAllocZeroed`] and the [`CBox`] / [`CArc`] constructors they drive.

#![allow(missing_docs)]
//!
//! Mock "C" routines over the Rust allocator record into counters, verifying
//! that:
//!
//! - every safe constructor allocates through the policy that frees it, and
//!   each allocation is released exactly once
//! - uninitialised storage dropped unfilled is released by the policy's
//!   `CDrop<MaybeUninit<T>>`; once filled, by its `CDrop<T>`
//! - a storage-only policy written as one generic `CDrop<T>` impl, the
//!   generic `free` shape, also serves the unfilled box
//! - `new_zeroed` hands out an all-zero object, and `CZeroable::zeroed` builds one
//! - a C failure surfaces as `None` and releases nothing
//! - the `_with` constructors keep the policy they were given

use core::mem::MaybeUninit;
use core::ptr::{addr_of, NonNull};
use core::sync::atomic::{AtomicUsize, Ordering};
use std::alloc::{alloc, alloc_zeroed, dealloc, Layout};
use std::sync::{Mutex, MutexGuard};

use ffibox::{define_ctype, CAlloc, CAllocZeroed, CArc, CBox, CCell, CDrop, CNew, CZeroable};

// The counters are process-global; tests take this lock so one test's reset
// cannot clobber another's tally (see `smart_pointers.rs`).
static LOCK: Mutex<()> = Mutex::new(());

static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static DEALLOCS: AtomicUsize = AtomicUsize::new(0);
static DROPS: AtomicUsize = AtomicUsize::new(0);
static NEWS: AtomicUsize = AtomicUsize::new(0);

fn reset() -> MutexGuard<'static, ()> {
    let guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    for c in [&ALLOCS, &DEALLOCS, &DROPS, &NEWS] {
        c.store(0, Ordering::SeqCst);
    }
    guard
}

fn count(c: &AtomicUsize) -> usize {
    c.load(Ordering::SeqCst)
}

mod ffi {
    #[repr(C)]
    pub struct plain_st {
        pub x: u32,
        pub next: *mut plain_st,
    }

    #[repr(C)]
    pub struct frame_st {
        pub format: i32,
    }
}

// A struct C documents as "zero, then fill": all-zero is a valid state.
define_ctype!(Plain, PlainRef, PlainMut, ffi::plain_st);
// SAFETY: `x == 0` and a null `next` are valid for every operation below, and
// `Heap`'s teardown never reads the object.
unsafe impl CZeroable for Plain {}

impl PlainRef<'_> {
    fn x(&self) -> u32 {
        // SAFETY: a read through the raw pointer; no reference is formed.
        unsafe { addr_of!((*self.as_ptr()).x).read() }
    }
    fn next_is_null(&self) -> bool {
        // SAFETY: as above.
        unsafe { addr_of!((*self.as_ptr()).next).read() }.is_null()
    }
}

// A struct whose C constructor sets a non-zero default: not
// `CZeroable`, so it is built only through its `CNew`.
define_ctype!(Frame, FrameRef, FrameMut, ffi::frame_st);

impl FrameRef<'_> {
    fn format(&self) -> i32 {
        // SAFETY: a read through the raw pointer; no reference is formed.
        unsafe { addr_of!((*self.as_ptr()).format).read() }
    }
}

// ---------------------------------------------------------------------------
// Heap — a storage policy over `Plain`, as `malloc` / `calloc` /
// `free`, with the two frees counted apart
// ---------------------------------------------------------------------------

/// `fail` makes every allocation report failure, as a NULL from C.
#[derive(Clone, Copy, Debug, Default)]
struct Heap {
    fail: bool,
}

fn heap_alloc<T>(zeroed: bool) -> *mut u8 {
    ALLOCS.fetch_add(1, Ordering::SeqCst);
    let layout = Layout::new::<T>();
    // SAFETY: the layout types here are not zero-sized.
    unsafe {
        if zeroed {
            alloc_zeroed(layout)
        } else {
            alloc(layout)
        }
    }
}

fn heap_free<T>(ptr: *mut u8) {
    // SAFETY: every caller passes storage `heap_alloc::<T>` produced.
    unsafe { dealloc(ptr, Layout::new::<T>()) }
}

// SAFETY: releases a filled allocation exactly once, without reading it.
unsafe impl CDrop<Plain> for Heap {
    unsafe fn c_drop(&self, ptr: NonNull<Plain>) {
        DROPS.fetch_add(1, Ordering::SeqCst);
        heap_free::<Plain>(ptr.as_ptr().cast());
    }
}

// SAFETY: releases unfilled storage exactly once, without reading it.
unsafe impl CDrop<MaybeUninit<Plain>> for Heap {
    unsafe fn c_drop(&self, ptr: NonNull<MaybeUninit<Plain>>) {
        DEALLOCS.fetch_add(1, Ordering::SeqCst);
        heap_free::<Plain>(ptr.as_ptr().cast());
    }
}

// SAFETY: a fresh allocation sized and aligned by `Layout::new`, which both
// `CDrop` impls release without reading.
unsafe impl CAlloc<Plain> for Heap {
    fn c_alloc(&self) -> Option<NonNull<MaybeUninit<Plain>>> {
        if self.fail {
            return None;
        }
        NonNull::new(heap_alloc::<Plain>(false).cast())
    }
}

// SAFETY: as `CAlloc`, all-zero, which `Plain` accepts per `CZeroable`.
unsafe impl CAllocZeroed<Plain> for Heap {
    fn c_alloc_zeroed(&self) -> Option<NonNull<Plain>> {
        if self.fail {
            return None;
        }
        NonNull::new(heap_alloc::<Plain>(true).cast())
    }
}

// ---------------------------------------------------------------------------
// GenericHeap — the same allocator written once for every `T`
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default)]
struct GenericHeap;

// SAFETY: releases storage `heap_alloc::<T>` produced exactly once, without
// reading it — so it serves a filled `T` and an unfilled `MaybeUninit<T>`
// alike (both have `T`'s layout).
unsafe impl<T> CDrop<T> for GenericHeap {
    unsafe fn c_drop(&self, ptr: NonNull<T>) {
        DROPS.fetch_add(1, Ordering::SeqCst);
        heap_free::<T>(ptr.as_ptr().cast());
    }
}

// SAFETY: as `Heap`'s.
unsafe impl<T: CCell> CAlloc<T> for GenericHeap {
    fn c_alloc(&self) -> Option<NonNull<MaybeUninit<T>>> {
        NonNull::new(heap_alloc::<T>(false).cast())
    }
}

// SAFETY: as `Heap`'s, for any `CZeroable` `T`.
unsafe impl<T: CZeroable> CAllocZeroed<T> for GenericHeap {
    fn c_alloc_zeroed(&self) -> Option<NonNull<T>> {
        NonNull::new(heap_alloc::<T>(true).cast())
    }
}

// ---------------------------------------------------------------------------
// FrameFree — a type-specific constructor / destructor pair
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default)]
struct FrameFree {
    fail: bool,
}

/// Mock `frame_alloc()` — allocates and sets the non-zero default.
fn frame_alloc() -> *mut ffi::frame_st {
    NEWS.fetch_add(1, Ordering::SeqCst);
    Box::into_raw(Box::new(ffi::frame_st { format: -1 }))
}

// SAFETY: frees a `frame_alloc` result exactly once.
unsafe impl CDrop<Frame> for FrameFree {
    unsafe fn c_drop(&self, ptr: NonNull<Frame>) {
        DROPS.fetch_add(1, Ordering::SeqCst);
        // SAFETY: `ptr` came from `frame_alloc`'s `Box`.
        drop(unsafe { Box::from_raw(ptr.as_ptr().cast::<ffi::frame_st>()) });
    }
}

// SAFETY: `frame_alloc` returns a fresh, initialised frame `c_drop` frees.
unsafe impl CNew<Frame> for FrameFree {
    fn c_new(&self) -> Option<NonNull<Frame>> {
        if self.fail {
            return None;
        }
        NonNull::new(frame_alloc().cast())
    }
}

type FrameBox = CBox<Frame, FrameFree>;

/// Mock `plain_init(p, x)`: a C routine initialising storage in place,
/// then the promotion its success justifies.
fn plain_init<D: CAlloc<Plain>>(
    mut storage: CBox<MaybeUninit<Plain>, D>,
    x: u32,
) -> CBox<Plain, D> {
    let p = storage.as_c_ptr();
    // SAFETY: `p` addresses storage for one `plain_st`, uniquely owned.
    unsafe {
        p.write(ffi::plain_st {
            x,
            next: core::ptr::null_mut(),
        })
    };
    // SAFETY: every field was just written with a valid value.
    unsafe { storage.assume_init() }
}
type PlainBox = CBox<Plain, Heap>;

// ---------------------------------------------------------------------------
// CNew
// ---------------------------------------------------------------------------

#[test]
fn new_runs_the_c_constructor_and_its_paired_free() {
    let _g = reset();
    let frame = FrameBox::new().unwrap();
    assert_eq!(frame.as_ref().format(), -1);
    assert_eq!(count(&NEWS), 1);
    drop(frame);
    assert_eq!(count(&DROPS), 1);
}

#[test]
fn new_failure_is_none_and_frees_nothing() {
    let _g = reset();
    assert!(FrameBox::new_with(FrameFree { fail: true }).is_none());
    assert_eq!(count(&NEWS), 0);
    assert_eq!(count(&DROPS), 0);
}

#[test]
fn new_with_keeps_the_given_policy() {
    let _g = reset();
    let frame = FrameBox::new_with(FrameFree { fail: false }).unwrap();
    assert!(!frame.policy().fail);
}

#[test]
fn arc_new_shares_the_fresh_reference() {
    let _g = reset();
    let frame = CArc::<Frame, FrameFree>::new().unwrap();
    assert_eq!(frame.as_ref().format(), -1);
    drop(frame);
    assert_eq!((count(&NEWS), count(&DROPS)), (1, 1));
}

// ---------------------------------------------------------------------------
// CAllocZeroed / CZeroable
// ---------------------------------------------------------------------------

#[test]
fn new_zeroed_hands_out_an_all_zero_object() {
    let _g = reset();
    let plain = PlainBox::new_zeroed().unwrap();
    assert_eq!(plain.as_ref().x(), 0);
    assert!(plain.as_ref().next_is_null());
    drop(plain);
    assert_eq!((count(&ALLOCS), count(&DROPS), count(&DEALLOCS)), (1, 1, 0));
}

#[test]
fn new_zeroed_failure_is_none() {
    let _g = reset();
    assert!(PlainBox::new_zeroed_with(Heap { fail: true }).is_none());
    assert!(CArc::<Plain, Heap>::new_zeroed_with(Heap { fail: true }).is_none());
    assert_eq!((count(&ALLOCS), count(&DROPS)), (0, 0));
}

#[test]
fn arc_new_zeroed_releases_once() {
    let _g = reset();
    let plain = CArc::<Plain, Heap>::new_zeroed().unwrap();
    assert_eq!(plain.as_ref().x(), 0);
    drop(plain);
    assert_eq!((count(&ALLOCS), count(&DROPS)), (1, 1));
}

#[test]
fn czeroable_zeroed_builds_a_value() {
    let v = <Plain as CZeroable>::zeroed();
    assert_eq!(v.as_ref().x(), 0);
    assert!(v.as_ref().next_is_null());
}

// ---------------------------------------------------------------------------
// CAlloc
// ---------------------------------------------------------------------------

#[test]
fn unfilled_storage_is_deallocated_not_dropped() {
    let _g = reset();
    let storage = PlainBox::new_uninit().unwrap();
    drop(storage);
    assert_eq!((count(&ALLOCS), count(&DEALLOCS), count(&DROPS)), (1, 1, 0));
}

#[test]
fn c_fills_through_as_c_ptr_then_assume_init() {
    let _g = reset();
    let plain = plain_init(PlainBox::new_uninit().unwrap(), 9);
    assert_eq!(plain.as_ref().x(), 9);
    drop(plain);
    assert_eq!((count(&DROPS), count(&DEALLOCS)), (1, 0));
}

#[test]
fn new_uninit_failure_is_none() {
    let _g = reset();
    assert!(PlainBox::new_uninit_with(Heap { fail: true }).is_none());
    assert_eq!((count(&ALLOCS), count(&DEALLOCS)), (0, 0));
}

#[test]
fn filled_box_converts_into_an_arc() {
    let _g = reset();
    let arc: CArc<Plain, Heap> = plain_init(PlainBox::new_uninit().unwrap(), 0).into();
    drop(arc);
    assert_eq!((count(&ALLOCS), count(&DROPS), count(&DEALLOCS)), (1, 1, 0));
}

#[test]
fn generic_storage_policy_serves_both_states() {
    let _g = reset();
    drop(CBox::<Plain, GenericHeap>::new_uninit().unwrap());
    let plain = plain_init(CBox::<Plain, GenericHeap>::new_uninit().unwrap(), 0);
    assert_eq!(plain.as_ref().x(), 0);
    drop(plain);
    drop(CArc::<Plain, GenericHeap>::new_zeroed().unwrap());
    assert_eq!((count(&ALLOCS), count(&DROPS)), (3, 3));
}

#[test]
fn uninit_box_has_the_initialised_layout() {
    use core::mem::size_of;
    assert_eq!(
        size_of::<CBox<MaybeUninit<Plain>, Heap>>(),
        size_of::<CBox<Plain, Heap>>()
    );
}
