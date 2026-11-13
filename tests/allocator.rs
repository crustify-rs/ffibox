//! `CAllocator`, the standard `Allocator` over a C allocation family, over a
//! stand-in family of `extern "C"` routines on `std::alloc::System`, so that
//! Miri runs it. The family records every buffer it hands out, so a test sees
//! each call C would get: a `free` of a pointer it never returned (a dangling
//! one) is counted, not performed, and so is a sized free given the wrong
//! size, a `malloc(0)` and a buffer never freed.

#![cfg(feature = "alloc")]
#![allow(missing_docs)]

use core::alloc::{AllocError, Allocator, Layout};
use core::ffi::{CStr, c_void};
use core::mem::size_of;
use core::ptr::{NonNull, null_mut};
use std::alloc::{GlobalAlloc, System};
use std::cell::RefCell;
use std::collections::HashMap;

use ffibox::{
    CAllocator, CBox, CLenAlloc, CStrBox, CVec, CZeroable, define_ctype, impl_clenalloc,
    impl_clendrop,
};

// ---------------------------------------------------------------------------
// The stand-in C family
// ---------------------------------------------------------------------------

/// Every buffer is preceded by a header this wide, which keeps it 16-aligned.
const HDR: usize = 16;

#[derive(Default)]
struct Track {
    /// Live buffers: address to size.
    live: HashMap<usize, usize>,
    mallocs: usize,
    zero_mallocs: usize,
    reallocs: usize,
    frees: usize,
    /// `free` or `realloc` of a pointer the family did not hand out.
    bad_frees: usize,
    /// A sized free given another size than the buffer's.
    size_mismatches: usize,
    fail_malloc: bool,
    fail_realloc: bool,
}

thread_local! {
    static TRACK: RefCell<Track> = RefCell::new(Track::default());
}

fn with<R>(f: impl FnOnce(&mut Track) -> R) -> R {
    TRACK.with(|t| f(&mut t.borrow_mut()))
}

/// What a test reads back.
#[derive(Debug, Default, PartialEq, Eq, Clone, Copy)]
struct Counts {
    live: usize,
    mallocs: usize,
    zero_mallocs: usize,
    reallocs: usize,
    frees: usize,
    bad_frees: usize,
    size_mismatches: usize,
}

fn counts() -> Counts {
    with(|t| Counts {
        live: t.live.len(),
        mallocs: t.mallocs,
        zero_mallocs: t.zero_mallocs,
        reallocs: t.reallocs,
        frees: t.frees,
        bad_frees: t.bad_frees,
        size_mismatches: t.size_mismatches,
    })
}

/// Starts a test on a clean family, and checks on drop that it ended with
/// nothing live and no bad call.
struct Fresh;

fn fresh() -> Fresh {
    with(|t| *t = Track::default());
    Fresh
}

impl Drop for Fresh {
    fn drop(&mut self) {
        if std::thread::panicking() {
            return;
        }
        let c = counts();
        assert_eq!(c.live, 0, "a buffer was never freed: {c:?}");
        assert_eq!(
            c.bad_frees, 0,
            "C was handed a pointer it never allocated: {c:?}"
        );
        assert_eq!(
            c.size_mismatches, 0,
            "a sized free got the wrong size: {c:?}"
        );
        assert_eq!(c.zero_mallocs, 0, "C's malloc was asked for 0 bytes: {c:?}");
    }
}

fn raw_layout(size: usize) -> Layout {
    Layout::from_size_align(size + HDR, 16).unwrap()
}

/// A fresh buffer, untracked.
unsafe fn raw_alloc(size: usize) -> *mut u8 {
    // SAFETY: the layout is never zero-sized.
    let base = unsafe { System.alloc(raw_layout(size)) };
    if base.is_null() {
        return null_mut();
    }
    // SAFETY: `HDR` bytes into a `size + HDR`-byte allocation.
    unsafe { base.add(HDR) }
}

/// # Safety
///
/// `p` must be a live `raw_alloc(size)` buffer.
unsafe fn raw_free(p: *mut u8, size: usize) {
    // SAFETY: the caller passes a buffer `raw_alloc` returned.
    unsafe { System.dealloc(p.sub(HDR), raw_layout(size)) }
}

/// The family's `malloc`.
unsafe extern "C" fn heap_malloc(size: usize) -> *mut c_void {
    if size == 0 {
        with(|t| t.zero_mallocs += 1);
    }
    if with(|t| t.fail_malloc) {
        return null_mut();
    }
    // SAFETY: any size.
    let p = unsafe { raw_alloc(size) };
    if !p.is_null() {
        with(|t| {
            t.live.insert(p.addr(), size);
            t.mallocs += 1;
        });
    }
    p.cast()
}

/// The family's `free`; NULL is a no-op, as for C's.
unsafe extern "C" fn heap_free(p: *mut c_void) {
    if p.is_null() {
        return;
    }
    let Some(size) = with(|t| t.live.remove(&p.addr())) else {
        with(|t| t.bad_frees += 1);
        return;
    };
    with(|t| t.frees += 1);
    // SAFETY: a live buffer of `size` bytes this family handed out.
    unsafe { raw_free(p.cast(), size) }
}

/// The family's `realloc`.
unsafe extern "C" fn heap_realloc(p: *mut c_void, size: usize) -> *mut c_void {
    if p.is_null() {
        // SAFETY: as `malloc`.
        return unsafe { heap_malloc(size) };
    }
    let Some(old) = with(|t| t.live.get(&p.addr()).copied()) else {
        with(|t| t.bad_frees += 1);
        return null_mut();
    };
    if size == 0 {
        with(|t| t.zero_mallocs += 1);
    }
    if with(|t| t.fail_realloc) {
        return null_mut();
    }
    // SAFETY: any size.
    let new = unsafe { raw_alloc(size) };
    if new.is_null() {
        return null_mut();
    }
    // SAFETY: `p` holds `old` bytes and `new` `size`, disjoint; `p` is freed
    // once, after.
    unsafe {
        core::ptr::copy_nonoverlapping(p.cast::<u8>(), new, old.min(size));
        raw_free(p.cast(), old);
    }
    with(|t| {
        t.live.remove(&p.addr());
        t.live.insert(new.addr(), size);
        t.reallocs += 1;
    });
    new.cast()
}

/// # Safety
///
/// `p` must be a buffer of the family.
unsafe fn heap_free_len(p: *mut u8, _byte_len: usize) {
    // SAFETY: the caller transfers the buffer.
    unsafe { heap_free(p.cast()) }
}

/// A free that takes the size: given another than the buffer's, it counts a
/// mismatch.
///
/// # Safety
///
/// `p` must be a buffer of the family.
unsafe fn heap_sized_free(p: *mut u8, byte_len: usize) {
    let size = with(|t| t.live.get(&p.addr()).copied());
    if size.is_some_and(|s| s != byte_len) {
        with(|t| t.size_mismatches += 1);
    }
    // SAFETY: the caller transfers the buffer.
    unsafe { heap_free(p.cast()) }
}

fn fail_malloc(on: bool) {
    with(|t| t.fail_malloc = on);
}

fn fail_realloc(on: bool) {
    with(|t| t.fail_realloc = on);
}

/// The family with its `realloc`, 16-aligned, its free ignoring the size.
#[derive(Clone, Copy, Debug, Default)]
struct Heap;
impl_clendrop!(Heap, heap_free_len);
impl_clenalloc!(Heap, heap_malloc, align = 16, realloc = heap_realloc);

/// The same family without a `realloc`: moves are `malloc`, copy, `free`.
#[derive(Clone, Copy, Debug, Default)]
struct HeapNoRealloc;
impl_clendrop!(HeapNoRealloc, heap_free_len);
impl_clenalloc!(HeapNoRealloc, heap_malloc, align = 16);

/// The family under a free that takes the size.
#[derive(Clone, Copy, Debug, Default)]
struct SizedHeap;
impl_clendrop!(SizedHeap, heap_sized_free);
impl_clenalloc!(SizedHeap, heap_malloc, align = 16, realloc = heap_realloc);

/// The family vouching for 8-byte alignment only.
#[derive(Clone, Copy, Debug, Default)]
struct Narrow;
impl_clendrop!(Narrow, heap_free_len);
impl_clenalloc!(Narrow, heap_malloc, align = 8, realloc = heap_realloc);

type A = CAllocator<Heap>;
const ALLOC: A = CAllocator::new(Heap);

#[repr(C, align(16))]
#[derive(Clone, Copy, Debug, PartialEq)]
struct Wide([u8; 16]);

fn layout(size: usize, align: usize) -> Layout {
    Layout::from_size_align(size, align).unwrap()
}

// ---------------------------------------------------------------------------
// The detector itself
// ---------------------------------------------------------------------------

#[test]
fn the_family_counts_a_free_of_a_dangling_pointer() {
    let _ = fresh();
    // SAFETY: the stand-in refuses a pointer it did not hand out, without
    // touching it.
    unsafe { heap_free(NonNull::<u64>::dangling().as_ptr().cast()) };
    assert_eq!(counts().bad_frees, 1);
    with(|t| t.bad_frees = 0);
}

// ---------------------------------------------------------------------------
// The allocator
// ---------------------------------------------------------------------------

#[test]
fn zero_sized_requests_never_reach_c() {
    let _f = fresh();
    for align in [1, 8, 16] {
        let l = layout(0, align);
        let block = ALLOC.allocate(l).unwrap();
        assert_eq!(block.len(), 0);
        assert_eq!(block.cast::<u8>().as_ptr().addr() % align, 0);
        // SAFETY: the block `allocate` just returned, with its layout.
        unsafe { ALLOC.deallocate(block.cast(), l) };
        let zeroed = ALLOC.allocate_zeroed(l).unwrap();
        // SAFETY: as above.
        unsafe { ALLOC.deallocate(zeroed.cast(), l) };
    }
    assert_eq!(counts(), Counts::default());
}

#[test]
fn blocks_are_the_size_requested_and_aligned() {
    let _f = fresh();
    for (size, align) in [(1, 1), (3, 2), (24, 8), (100, 16)] {
        let l = layout(size, align);
        let block = ALLOC.allocate(l).unwrap();
        assert_eq!(block.len(), size);
        assert_eq!(block.cast::<u8>().as_ptr().addr() % align, 0);
        // SAFETY: the block `allocate` just returned, with its layout.
        unsafe { ALLOC.deallocate(block.cast(), l) };
    }
    let c = counts();
    assert_eq!((c.mallocs, c.frees), (4, 4));
}

#[test]
fn alignment_beyond_the_family_is_an_alloc_error() {
    let _f = fresh();
    let narrow = CAllocator::new(Narrow);
    assert_eq!(narrow.allocate(layout(16, 16)), Err(AllocError));
    assert_eq!(narrow.allocate(layout(0, 16)), Err(AllocError));
    assert_eq!(ALLOC.allocate(layout(64, 32)), Err(AllocError));
    assert_eq!(ALLOC.allocate_zeroed(layout(64, 32)), Err(AllocError));

    // A move to an alignment beyond it fails and leaves the block alone.
    let l = layout(8, 8);
    let block = narrow.allocate(l).unwrap().cast::<u8>();
    // SAFETY: `block` is live under `l`.
    unsafe {
        block.as_ptr().write_bytes(7, 8);
        assert_eq!(narrow.grow(block, l, layout(32, 16)), Err(AllocError));
        assert_eq!(narrow.shrink(block, l, layout(4, 16)), Err(AllocError));
        assert_eq!(*block.as_ptr().add(7), 7);
        narrow.deallocate(block, l);
    }
    let c = counts();
    assert_eq!((c.mallocs, c.reallocs, c.frees), (1, 0, 1));
}

#[test]
fn allocation_failure_is_an_alloc_error() {
    let _f = fresh();
    fail_malloc(true);
    assert_eq!(ALLOC.allocate(layout(8, 8)), Err(AllocError));
    assert!(ALLOC.try_box(5u64).is_err());
    let mut v: Vec<u8, A> = Vec::new_in(ALLOC);
    assert!(v.try_reserve(10).is_err());
    fail_malloc(false);
    assert_eq!(counts().mallocs, 0);
}

#[test]
fn grow_and_shrink_go_through_realloc() {
    let _f = fresh();
    let small = layout(4, 4);
    let big = layout(64, 4);
    let p = ALLOC.allocate(small).unwrap().cast::<u8>();
    // SAFETY: each block is used under the layout that fits it.
    unsafe {
        p.as_ptr()
            .copy_from_nonoverlapping([1, 2, 3, 4].as_ptr(), 4);
        let p = ALLOC.grow(p, small, big).unwrap();
        assert_eq!(p.len(), 64);
        let p = p.cast::<u8>();
        assert_eq!(core::slice::from_raw_parts(p.as_ptr(), 4), [1, 2, 3, 4]);
        let p = ALLOC.shrink(p, big, layout(2, 2)).unwrap().cast::<u8>();
        assert_eq!(core::slice::from_raw_parts(p.as_ptr(), 2), [1, 2]);
        // The same size keeps the block, without a call.
        let same = ALLOC.shrink(p, layout(2, 2), layout(2, 1)).unwrap();
        assert_eq!(same.cast::<u8>(), p);
        ALLOC.deallocate(p, layout(2, 1));
    }
    let c = counts();
    assert_eq!((c.mallocs, c.reallocs, c.frees), (1, 2, 1));
}

#[test]
fn a_family_without_realloc_moves_through_malloc_and_free() {
    let _f = fresh();
    let a = CAllocator::new(HeapNoRealloc);
    let mut v: Vec<u32, CAllocator<HeapNoRealloc>> = Vec::new_in(a);
    v.extend(0..100);
    v.shrink_to_fit();
    assert_eq!(v.iter().sum::<u32>(), 4950);
    drop(v);
    let c = counts();
    assert_eq!(c.reallocs, 0);
    assert_eq!(c.mallocs, c.frees);
}

#[test]
fn a_failed_move_leaves_the_block_as_it_was() {
    let _f = fresh();
    let l = layout(8, 8);
    let p = ALLOC.allocate(l).unwrap().cast::<u8>();
    fail_realloc(true);
    // SAFETY: `p` is live under `l` throughout.
    unsafe {
        p.as_ptr().write_bytes(9, 8);
        assert_eq!(ALLOC.grow(p, l, layout(80, 8)), Err(AllocError));
        assert_eq!(ALLOC.grow_zeroed(p, l, layout(80, 8)), Err(AllocError));
        assert_eq!(ALLOC.shrink(p, l, layout(4, 8)), Err(AllocError));
        assert_eq!(core::slice::from_raw_parts(p.as_ptr(), 8), [9; 8]);
        fail_realloc(false);
        ALLOC.deallocate(p, l);
    }

    // Through a `Vec`: `try_reserve` fails and the elements stay.
    let mut v: Vec<u64, A> = Vec::new_in(ALLOC);
    v.extend([1, 2, 3]);
    v.shrink_to_fit();
    fail_realloc(true);
    assert!(v.try_reserve(1000).is_err());
    fail_realloc(false);
    assert_eq!(v, [1, 2, 3]);
}

#[test]
fn grow_zeroed_zeroes_past_the_old_size() {
    let _f = fresh();
    let old = layout(3, 1);
    let new = layout(40, 1);
    let p = ALLOC.allocate(old).unwrap().cast::<u8>();
    // SAFETY: each block is used under the layout that fits it.
    unsafe {
        p.as_ptr().write_bytes(0xAA, 3);
        let q = ALLOC.grow_zeroed(p, old, new).unwrap().cast::<u8>();
        let bytes = core::slice::from_raw_parts(q.as_ptr(), 40);
        assert_eq!(bytes[..3], [0xAA; 3]);
        assert!(bytes[3..].iter().all(|&b| b == 0));
        ALLOC.deallocate(q, new);

        // From a block of size 0, every byte is zeroed, by a fresh `malloc`.
        let z = ALLOC.allocate(layout(0, 1)).unwrap().cast::<u8>();
        let q = ALLOC
            .grow_zeroed(z, layout(0, 1), new)
            .unwrap()
            .cast::<u8>();
        assert!(
            core::slice::from_raw_parts(q.as_ptr(), 40)
                .iter()
                .all(|&b| b == 0)
        );
        ALLOC.deallocate(q, new);
    }
}

#[test]
fn shrinking_to_zero_frees_and_returns_a_dangling_block() {
    let _f = fresh();
    let l = layout(16, 8);
    let p = ALLOC.allocate(l).unwrap().cast::<u8>();
    // SAFETY: `p` is live under `l`; the block of size 0 is never freed by C.
    unsafe {
        let z = ALLOC.shrink(p, l, layout(0, 8)).unwrap();
        assert_eq!(z.len(), 0);
        ALLOC.deallocate(z.cast(), layout(0, 8));
    }
    let c = counts();
    assert_eq!((c.mallocs, c.frees), (1, 1));
}

#[test]
fn every_instance_of_a_stateless_family_is_one_allocator() {
    let _f = fresh();
    let l = layout(8, 8);
    let p = CAllocator::new(Heap).allocate(l).unwrap();
    // SAFETY: another instance of a stateless family frees what one allocated.
    unsafe { CAllocator::<Heap>::default().deallocate(p.cast(), l) };
}

// ---------------------------------------------------------------------------
// Box and Vec over it
// ---------------------------------------------------------------------------

#[test]
fn a_box_is_one_pointer_with_c_s_abi() {
    assert_eq!(size_of::<A>(), 0);
    assert_eq!(size_of::<Box<u64, A>>(), size_of::<*mut u64>());
    assert_eq!(size_of::<Option<Box<u64, A>>>(), size_of::<*mut u64>());
    assert_eq!(size_of::<Vec<u64, A>>(), size_of::<Vec<u64>>());
    fn copy<T: Copy>() {}
    copy::<A>();
}

#[test]
fn the_allocator_crosses_threads_as_its_family_does() {
    let _f = fresh();
    fn send_sync<T: Send + Sync>() {}
    send_sync::<A>();
    send_sync::<Box<u64, A>>();
    send_sync::<Vec<u64, A>>();

    /// A family that frees on the allocating thread only.
    #[derive(Clone, Copy, Default)]
    struct Local(core::marker::PhantomData<*const ()>);
    impl_clendrop!(Local, heap_free_len);
    impl_clenalloc!(Local, heap_malloc, align = 16);
    let b = Box::new_in(1u64, CAllocator::new(Local(core::marker::PhantomData)));
    drop(b);
    // `CAllocator<Local>`, and so a box over it, is neither `Send` nor
    // `Sync`: the compile-fail doc test on `CAllocator` checks it.
}

#[test]
fn boxes_allocate_and_free_through_the_family() {
    let _f = fresh();
    let b = Box::new_in(41u64, ALLOC);
    assert_eq!(*b + 1, 42);
    let c = b.clone();
    drop(b);
    drop(c);
    let w = ALLOC.try_box(Wide([3; 16])).unwrap();
    assert_eq!(core::ptr::from_ref(&*w).addr() % 16, 0);
    drop(w);
    let n = counts();
    assert_eq!((n.mallocs, n.frees), (3, 3));
}

#[test]
fn a_box_of_a_zero_sized_type_allocates_nothing() {
    let _f = fresh();
    let b = Box::new_in((), ALLOC);
    let t = ALLOC.try_box(()).unwrap();
    drop((b, t));
    assert_eq!(counts(), Counts::default());
}

#[test]
fn vectors_grow_through_realloc_and_free_once() {
    let _f = fresh();
    let mut v: Vec<u32, A> = Vec::new_in(ALLOC);
    assert_eq!(counts().mallocs, 0, "an empty Vec allocates nothing");
    for i in 0..1000 {
        v.push(i);
    }
    assert_eq!(v.len(), 1000);
    v.truncate(3);
    v.shrink_to_fit();
    assert_eq!(v, [0, 1, 2]);
    v.clear();
    v.shrink_to_fit(); // frees, back to the dangling empty Vec
    assert_eq!(counts().live, 0);
    v.push(9);
    drop(v);
    let c = counts();
    assert_eq!(c.mallocs, 2);
    assert!(c.reallocs > 0);
    assert_eq!(c.frees, 2);
}

#[test]
fn a_vector_of_zero_sized_elements_allocates_nothing() {
    let _f = fresh();
    let mut v: Vec<(), A> = Vec::new_in(ALLOC);
    v.extend([(); 100]);
    assert_eq!(v.len(), 100);
    drop(v);
    assert_eq!(counts(), Counts::default());
}

#[test]
fn a_sized_free_gets_each_block_s_exact_size() {
    let _f = fresh();
    let a = CAllocator::new(SizedHeap);
    let mut v: Vec<u16, CAllocator<SizedHeap>> = Vec::with_capacity_in(5, a);
    v.extend(0..40);
    v.truncate(7);
    v.shrink_to_fit();
    drop(v);
    let b = Box::new_in([1u8; 3], a);
    drop(b);
}

#[test]
fn a_vector_adopts_a_buffer_c_allocated_without_a_capacity() {
    let _f = fresh();
    // C allocates four `u32`s and records only their count.
    // SAFETY: the family's `malloc`; the buffer is filled before adoption.
    let ptr = unsafe { heap_malloc(4 * size_of::<u32>()) }.cast::<u32>();
    // SAFETY: as above.
    unsafe {
        for i in 0..4 {
            ptr.add(i).write(i as u32 * 10);
        }
    }
    // SAFETY: a buffer of the family holding four `u32`s; `free` ignores the
    // size, so a capacity of four fits it (see `CAllocator`'s fit rule).
    let mut v = unsafe { Vec::from_raw_parts_in(ptr, 4, 4, ALLOC) };
    v.push(40); // `realloc`
    assert_eq!(v, [0, 10, 20, 30, 40]);
    drop(v);
    let c = counts();
    assert_eq!((c.mallocs, c.reallocs, c.frees), (1, 1, 1));
}

// ---------------------------------------------------------------------------
// As a policy
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct point_st {
    x: i64,
    y: i64,
}
define_ctype!(Point, PointRef, PointMut, point_st);
// SAFETY: any coordinates are valid, zero included.
unsafe impl CZeroable for Point {}

impl PointRef<'_> {
    fn x(&self) -> i64 {
        use ffibox::CRefSeam;
        // SAFETY: a raw read through the shared handle.
        unsafe { (*self.as_ptr()).x }
    }
}

#[test]
fn as_a_policy_it_serves_boxes_strings_and_buffers() {
    let _f = fresh();
    let p: CBox<Point, A> = CBox::new().unwrap();
    assert_eq!(p.as_ref().x(), 0);
    let z: CBox<Point, A> = CBox::new_zeroed().unwrap();
    let mut u = CBox::<Point, A>::new_uninit().unwrap();
    // SAFETY: storage for one `point_st`, filled whole before promotion.
    unsafe { u.as_c_ptr().write(point_st { x: 7, y: 0 }) };
    // SAFETY: every field was written above.
    let u = unsafe { u.assume_init() };
    assert_eq!(u.as_ref().x(), 7);
    let unfilled = CBox::<Point, A>::new_uninit().unwrap();
    drop((p, z, u, unfilled));

    let s: CStrBox<A> = CStrBox::from_c_str(c"hello").unwrap();
    assert_eq!(s.as_c_str(), c"hello");
    drop(s);

    let v: CVec<u32, A> = CVec::from_fn(3, |i| i as u32).unwrap();
    assert_eq!(v.as_slice(), [0, 1, 2]);
    drop(v);
    let c = counts();
    assert_eq!(c.mallocs, 6);
    assert_eq!(c.frees, 6);

    // Strings under a sized free are freed with `strlen + 1`.
    let _g = fresh();
    let s: CStrBox<CAllocator<SizedHeap>> = CStrBox::from_c_str(c"sized").unwrap();
    let t = s.clone();
    assert_eq!(&*t, c"sized");
    let _ = CStr::to_bytes(&t);
}

#[test]
fn the_policy_forwards_the_family_s_buffer_routines() {
    assert_eq!(<A as CLenAlloc>::ALIGN, 16);
    assert_eq!(<CAllocator<Narrow> as CLenAlloc>::ALIGN, 8);
}

// ---------------------------------------------------------------------------
// To and from CBox and CVec
// ---------------------------------------------------------------------------

#[test]
fn a_box_becomes_a_cbox_and_back_without_copying() {
    let _f = fresh();
    let b = Box::new_in(Point::zeroed(), ALLOC);
    let at = core::ptr::from_ref(&*b).addr();
    let c: CBox<Point, A> = CBox::from(b);
    assert_eq!(c.as_ptr().addr(), at);
    assert_eq!(c.as_ref().x(), 0);
    // SAFETY: `Point` is `CZeroable`, so not address-sensitive, and nothing
    // but the box reaches it.
    let b = unsafe { c.into_box() };
    assert_eq!(core::ptr::from_ref(&*b).addr(), at);
    drop(b);
    let n = counts();
    assert_eq!((n.mallocs, n.frees), (1, 1));
}

#[test]
fn a_box_handed_to_c_is_freed_by_c_s_free() {
    let _f = fresh();
    let c: CBox<Point, A> = CBox::from(Box::new_in(Point::zeroed(), ALLOC));
    let raw = c.into_c();
    // SAFETY: C frees what the family allocated.
    unsafe { heap_free(raw.cast()) };
    let n = counts();
    assert_eq!((n.mallocs, n.frees), (1, 1));
}

#[test]
fn a_cbox_built_by_the_policy_becomes_a_box() {
    let _f = fresh();
    let c: CBox<Point, CAllocator<SizedHeap>> = CBox::new_zeroed().unwrap();
    // SAFETY: as above.
    let b = unsafe { c.into_box() };
    drop(b); // the sized free is given `size_of::<Point>()`
}

#[test]
fn a_cvec_becomes_a_vec_that_grows_in_place() {
    let _f = fresh();
    let v: CVec<u32, A> = CVec::from_fn(3, |i| i as u32 + 1).unwrap();
    let at = v.as_ptr().addr();
    let mut w: Vec<u32, A> = Vec::from(v);
    assert_eq!(w.as_ptr().addr(), at);
    assert_eq!((w.len(), w.capacity()), (3, 3));
    w.push(4); // `realloc`
    assert_eq!(w, [1, 2, 3, 4]);
    drop(w);
    let n = counts();
    assert_eq!((n.mallocs, n.reallocs, n.frees), (1, 1, 1));
}

#[test]
fn an_empty_cvec_becomes_a_vec_that_holds_nothing() {
    let _f = fresh();
    let w: Vec<u32, A> = Vec::from(CVec::<u32, A>::empty());
    assert_eq!(w.capacity(), 0);
    drop(w);

    // A non-NULL buffer of length 0, as `malloc(0)` may return, is freed
    // rather than leaked by a `Vec` of capacity 0.
    // SAFETY: a one-byte buffer of the family, adopted with no elements.
    let empty = unsafe {
        let p = heap_malloc(1).cast::<u32>();
        CVec::<u32, A>::from_raw_parts(p, 0).unwrap()
    };
    let w: Vec<u32, A> = Vec::from(empty);
    assert_eq!(w.capacity(), 0);
    assert_eq!(counts().live, 0);
    drop(w);
    let n = counts();
    assert_eq!((n.mallocs, n.frees), (1, 1));
}

#[test]
fn an_empty_vec_hands_c_null_never_its_dangling_pointer() {
    let _f = fresh();
    // Never allocated: capacity 0, a dangling pointer.
    let v: Vec<u64, A> = Vec::new_in(ALLOC);
    assert_eq!(v.capacity(), 0);
    let c = CVec::try_from(v).unwrap();
    let (ptr, len) = c.into_raw_parts();
    assert!(ptr.is_null());
    assert_eq!(len, 0);
    // SAFETY: C frees what the `CVec` gave it; `free(NULL)` does nothing.
    unsafe { heap_free(ptr.cast()) };

    // Allocated, then emptied: the buffer is freed and C gets NULL.
    let mut v: Vec<u64, A> = Vec::with_capacity_in(8, ALLOC);
    v.push(1);
    v.clear();
    let c = CVec::try_from(v).unwrap();
    assert!(c.as_ptr().is_null());
    assert_eq!(counts().live, 0);
    let (ptr, _) = c.into_raw_parts();
    // SAFETY: as above.
    unsafe { heap_free(ptr.cast()) };
    let n = counts();
    assert_eq!((n.mallocs, n.frees), (1, 1));
}

#[test]
fn a_vec_with_spare_capacity_is_shrunk_to_its_length() {
    let _f = fresh();
    let a = CAllocator::new(SizedHeap);
    let mut v: Vec<u32, CAllocator<SizedHeap>> = Vec::with_capacity_in(16, a);
    v.extend([5, 6, 7]);
    let c = CVec::try_from(v).unwrap();
    assert_eq!(c.as_slice(), [5, 6, 7]);
    assert_eq!(counts().reallocs, 1);
    // The sized free is given the buffer's exact size, 3 elements' worth.
    drop(c);

    // Handed to C, its `free` takes the buffer.
    let mut v: Vec<u32, A> = Vec::with_capacity_in(16, ALLOC);
    v.extend([1, 2]);
    let (ptr, len) = CVec::try_from(v).unwrap().into_raw_parts();
    assert_eq!(len, 2);
    // SAFETY: C frees what the family allocated.
    unsafe { heap_free(ptr.cast()) };
}

#[test]
fn a_vec_whose_shrink_fails_comes_back_unchanged() {
    let _f = fresh();
    let mut v: Vec<u32, A> = Vec::with_capacity_in(16, ALLOC);
    v.extend([1, 2, 3]);
    let at = v.as_ptr().addr();
    fail_realloc(true);
    let v = CVec::try_from(v).unwrap_err();
    fail_realloc(false);
    assert_eq!(v, [1, 2, 3]);
    assert_eq!((v.capacity(), v.as_ptr().addr()), (16, at));
    // A Vec already its length's size needs no shrink.
    let mut w: Vec<u32, A> = Vec::with_capacity_in(3, ALLOC);
    w.extend([1, 2, 3]);
    fail_realloc(true);
    let c = CVec::try_from(w).unwrap();
    fail_realloc(false);
    assert_eq!(c.as_slice(), [1, 2, 3]);
}

#[test]
fn owner_elements_cross_without_being_dropped_twice() {
    let _f = fresh();
    let strings: CVec<Option<CStrBox<A>>, A> =
        CVec::from_fn(3, |i| CStrBox::from_c_str([c"a", c"b", c"c"][i])).unwrap();
    let mut w: Vec<Option<CStrBox<A>>, A> = Vec::from(strings);
    w.push(None);
    w.push(CStrBox::from_c_str(c"d"));
    let back = CVec::try_from(w).unwrap();
    assert_eq!(back.len(), 5);
    assert_eq!(back.as_slice()[4].as_deref(), Some(c"d"));
    drop(back);
    let n = counts();
    assert_eq!(n.mallocs, n.frees);
}

// ---------------------------------------------------------------------------
// Review fixes: the abort guard, adoption of a larger allocation, a sized free
// ---------------------------------------------------------------------------

/// A family whose `malloc` panics, as a Rust-implemented one might.
#[derive(Clone, Copy, Debug, Default)]
struct Panicky;
impl_clendrop!(Panicky, heap_free_len);
// SAFETY: never returns a buffer: it panics.
unsafe impl CLenAlloc for Panicky {
    const ALIGN: usize = 16;
    fn c_alloc_len(&self, _byte_len: usize) -> Option<NonNull<u8>> {
        panic!("the family's malloc panicked");
    }
}

/// Set in the child process `a_panicking_family_aborts_rather_than_unwinds`
/// spawns, which runs the panicking allocation.
const ABORT_CHILD: &str = "FFIBOX_ALLOCATOR_ABORT_CHILD";

#[test]
#[cfg(unix)]
#[cfg_attr(miri, ignore = "spawns a process")]
fn a_panicking_family_aborts_rather_than_unwinds() {
    use std::os::unix::process::ExitStatusExt;
    if std::env::var_os(ABORT_CHILD).is_some() {
        // The child: unwinding out of `allocate` would fail this test with a
        // panic; the guard aborts the process first.
        let _ = CAllocator::new(Panicky).allocate(layout(8, 8));
        return;
    }
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "a_panicking_family_aborts_rather_than_unwinds",
            "--exact",
            "--test-threads=1",
        ])
        .env(ABORT_CHILD, "1")
        .output()
        .unwrap();
    assert_eq!(
        out.status.signal(),
        Some(6), // SIGABRT
        "the allocation unwound instead of aborting: {:?}\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn a_vector_adopts_a_buffer_larger_than_its_length() {
    let _f = fresh();
    // C allocates room for sixteen `u32`s and fills three: its allocation is
    // larger than the length it hands over, which a free that ignores the
    // size accepts as the Vec's capacity.
    // SAFETY: the family's `malloc`; the first three elements are written
    // before adoption.
    let ptr = unsafe { heap_malloc(16 * size_of::<u32>()) }.cast::<u32>();
    // SAFETY: as above.
    unsafe {
        for i in 0..3 {
            ptr.add(i).write(i as u32);
        }
    }
    // SAFETY: a buffer of the family C gave up, three `u32`s long, at most its
    // allocation, under a free that ignores the size.
    let mut v = unsafe { Vec::from_raw_parts_in(ptr, 3, 3, ALLOC) };
    v.push(3);
    v.shrink_to_fit();
    assert_eq!(v, [0, 1, 2, 3]);
    drop(v);

    // The same through a `CVec`, adopted with its length only.
    // SAFETY: as above.
    let ptr = unsafe { heap_malloc(16 * size_of::<u32>()) }.cast::<u32>();
    // SAFETY: as above.
    let c = unsafe {
        ptr.write(7);
        CVec::<u32, A>::from_raw_parts(ptr, 1).unwrap()
    };
    let mut w = Vec::from(c);
    w.extend([8, 9]);
    assert_eq!(w, [7, 8, 9]);
    drop(w);
    let n = counts();
    assert_eq!(n.mallocs, n.frees);
}

#[test]
fn an_emptied_vec_under_a_sized_free_is_freed_with_its_size() {
    let _f = fresh();
    let a = CAllocator::new(SizedHeap);
    let mut v: Vec<u64, CAllocator<SizedHeap>> = Vec::with_capacity_in(8, a);
    v.extend([1, 2, 3]);
    v.clear();
    let c = CVec::try_from(v).unwrap();
    assert!(c.as_ptr().is_null());
    // The sized free was given the 8 elements' bytes the Vec held.
    let n = counts();
    assert_eq!((n.mallocs, n.frees, n.size_mismatches), (1, 1, 0));
}
