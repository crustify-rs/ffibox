//! `CVec` over a NULL buffer: C's empty array. NULL with length 0 adopts as an
//! empty buffer through `from_raw_parts_or_empty`, frees nothing, hands NULL
//! back to C, and keeps the `{ T *ptr; size_t len; }` layout — while a
//! non-null zero-length buffer is still freed.

#![allow(missing_docs)]

use core::ptr::{null_mut, NonNull};
use core::sync::atomic::{AtomicUsize, Ordering};

use ffibox::{define_ctype, CLenClone, CLenDrop, CVec};

/// Counts frees into a counter each test owns. Never frees the memory: the one
/// test with a real buffer reclaims it itself, after checking the count.
#[derive(Clone, Copy)]
struct Count {
    frees: &'static AtomicUsize,
}

// SAFETY: frees nothing, so any pointer and length are accepted.
unsafe impl CLenDrop for Count {
    unsafe fn c_drop_len(&self, _: *mut u8, _: usize) {
        self.frees.fetch_add(1, Ordering::SeqCst);
    }
}
// SAFETY: never returns; only empty buffers are cloned here, and those must
// not reach C. So it returns no allocation, and every one it returns is
// aligned for any element type these tests clone.
unsafe impl CLenClone for Count {
    const ALIGN: usize = 8;

    unsafe fn c_clone_len(&self, _: *mut u8, _: usize) -> Option<NonNull<u8>> {
        unreachable!("an empty buffer clones without calling C")
    }
}

macro_rules! counters {
    () => {{
        static FREES: AtomicUsize = AtomicUsize::new(0);
        Count { frees: &FREES }
    }};
}

#[derive(Clone, Copy, Debug, Default)]
struct NoFree;
// SAFETY: frees nothing.
unsafe impl CLenDrop for NoFree {
    unsafe fn c_drop_len(&self, _: *mut u8, _: usize) {}
}

#[test]
fn null_with_length_zero_is_an_empty_buffer_that_frees_nothing() {
    let c = counters!();
    // SAFETY: NULL with length 0 owns nothing.
    let v = unsafe { CVec::<u32, Count>::from_raw_parts_or_empty_with(null_mut(), 0, c) }
        .expect("NULL with length 0 is empty, not a failure");
    assert!(v.is_empty());
    assert_eq!(v.byte_len(), 0);
    assert!(v.as_ptr().is_null(), "NULL goes back to C as NULL");
    assert_eq!(v.as_slice(), &[] as &[u32]);
    drop(v);
    assert_eq!(c.frees.load(Ordering::SeqCst), 0);
}

#[test]
fn null_with_a_length_is_still_rejected() {
    let c = counters!();
    // SAFETY: rejected before anything is adopted.
    let v = unsafe { CVec::<u32, Count>::from_raw_parts_or_empty_with(null_mut(), 3, c) };
    assert!(v.is_none());
    // `from_raw_parts` keeps treating every NULL as failure.
    // SAFETY: as above.
    assert!(unsafe { CVec::<u32, NoFree>::from_raw_parts(null_mut(), 0) }.is_none());
}

#[test]
fn a_non_null_zero_length_buffer_is_still_freed() {
    let c = counters!();
    // Stands in for `malloc(0)`: a live, non-null allocation.
    let p = Box::into_raw(Box::new(0u32));
    // SAFETY: a live allocation the policy accepts, with no elements in use.
    let v = unsafe { CVec::<u32, Count>::from_raw_parts_or_empty_with(p, 0, c) }.unwrap();
    assert!(v.is_empty());
    assert_eq!(v.as_ptr(), p);
    drop(v);
    assert_eq!(c.frees.load(Ordering::SeqCst), 1);
    // SAFETY: `Count` freed nothing; the allocation is still ours.
    drop(unsafe { Box::from_raw(p) });
}

#[test]
fn an_empty_buffer_round_trips_null_and_clones_without_calling_c() {
    let c = counters!();
    let v = CVec::<u32, Count>::empty_with(c);
    let w = v.clone();
    assert!(w.as_ptr().is_null() && w.is_empty());
    assert_eq!(v.into_raw_parts(), (null_mut(), 0));
    drop(w);
    assert_eq!(c.frees.load(Ordering::SeqCst), 0);

    let d: CVec<u8, NoFree> = CVec::default();
    assert!(d.as_ptr().is_null() && d.is_empty());
    let mut e = CVec::<u8, NoFree>::empty();
    assert!(e.as_mut_slice().is_empty());
}

#[repr(C)]
pub struct obj_st {
    x: u32,
}
define_ctype!(Obj, ObjRef, ObjMut, obj_st);

#[test]
fn an_empty_buffer_of_wrapped_objects_yields_empty_runs() {
    let mut v = CVec::<Obj, NoFree>::empty();
    assert!(v.as_handles().is_empty());
    assert_eq!(v.as_handles().iter().count(), 0);
    let mut run = v.as_handles_mut();
    assert!(run.get_mut(0).is_none());
}

#[test]
fn the_layout_is_cs_pointer_and_length() {
    use core::mem::size_of;
    #[repr(C)]
    struct CArray {
        ptr: *mut u32,
        len: usize,
    }
    assert_eq!(size_of::<CVec<u32, NoFree>>(), size_of::<CArray>());
    // NULL is now an in-range value, so `Option<CVec>` needs its own tag.
    assert!(size_of::<Option<CVec<u32, NoFree>>>() > size_of::<CArray>());
}
