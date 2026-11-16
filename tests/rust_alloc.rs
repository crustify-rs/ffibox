//! `RustAlloc`, the policy over Rust's global allocator, serves the owners
//! an object or buffer C allocates is held by, and the export macros offer
//! it back to C as routines.

#![allow(missing_docs)]

use core::ffi::c_char;
use core::ptr::{NonNull, null_mut};

use ffibox::{
    CArc, CBox, CDrop, CDupClone, CMutSeam, CRefSeam, CStrBox, CVec, CZeroable, RustAlloc,
    define_ctype,
};

#[repr(C)]
pub struct point_st {
    x: i32,
    y: i32,
}
define_ctype!(Point, PointRef, PointMut, point_st);
// SAFETY: any coordinates are valid, zero included.
unsafe impl CZeroable for Point {}

impl PointRef<'_> {
    fn x(&self) -> i32 {
        // SAFETY: a raw read through the shared handle.
        unsafe { (*self.as_ptr()).x }
    }
}
impl PointMut<'_> {
    fn set_x(&mut self, v: i32) {
        // SAFETY: a raw write through the exclusive handle.
        unsafe { (*self.as_mut_ptr()).x = v }
    }
}

type PointBox = CBox<Point, RustAlloc>;

#[test]
fn objects_are_built_zeroed_or_filled_in_place() {
    let mut p = PointBox::new().unwrap();
    assert_eq!(p.as_ref().x(), 0);
    p.as_mut().set_x(4);
    assert_eq!(p.as_ref().x(), 4);

    let z = PointBox::new_zeroed().unwrap();
    assert_eq!(z.as_ref().x(), 0);

    let mut u = PointBox::new_uninit().unwrap();
    // SAFETY: storage for one `point_st`, filled whole before promotion.
    unsafe {
        u.as_c_ptr()
            .cast::<point_st>()
            .write(point_st { x: 9, y: 1 })
    };
    // SAFETY: every field was written above.
    let u: PointBox = unsafe { u.assume_init() };
    assert_eq!(u.as_ref().x(), 9);
}

#[test]
fn an_object_shared_in_an_arc_is_freed_once() {
    let a: CArc<Point, RustAlloc> = CArc::from(PointBox::new().unwrap());
    assert_eq!(a.as_ref().x(), 0);
    // `RustAlloc` has no up_ref, so the arc is not `Clone`; it frees the
    // point once on drop.
    drop(a);
}

#[test]
fn strings_copy_in_and_clone() {
    let s: CStrBox<RustAlloc> = CStrBox::from_c_str(c"hello").unwrap();
    assert_eq!(s.to_str(), Ok("hello"));
    let t = s.clone();
    assert_eq!(t.as_c_str(), c"hello");
    assert_ne!(t.as_ptr(), s.as_ptr());
    let empty: CStrBox<RustAlloc> = CStrBox::from_c_str(c"").unwrap();
    assert!(empty.is_empty());
}

#[test]
fn buffers_copy_in_and_clone() {
    let v: CVec<u32, RustAlloc> = CVec::from_slice(&[1, 2, 3]).unwrap();
    let w = v.clone();
    assert_eq!(w.as_slice(), &[1, 2, 3]);
    let e: CVec<u32, RustAlloc> = CVec::from_slice(&[]).unwrap();
    assert!(e.as_ptr().is_null());
}

#[test]
fn out_parameters_are_adopted_whatever_the_routine_returns() {
    /// Stands in for `int point_open(point_st **out, int fail)`.
    unsafe fn point_open(out: *mut *mut point_st, fail: bool) -> i32 {
        let p = PointBox::new().unwrap().into_c();
        // SAFETY: the caller passes a writable slot.
        unsafe { *out = p };
        if fail { -1 } else { 0 }
    }
    // SAFETY: `point_open` stores an owned `RustAlloc` point.
    let (rc, p) = unsafe { PointBox::from_c_out(|out| point_open(out, false)) };
    assert_eq!(rc, 0);
    assert!(p.is_some());
    // An object handed over on failure is still adopted, and so freed.
    // SAFETY: as above.
    let (rc, p) = unsafe { PointBox::from_c_out(|out| point_open(out, true)) };
    assert_eq!((rc, p.is_some()), (-1, true));
    // A routine that stores nothing leaves the slot NULL.
    // SAFETY: the slot is left untouched.
    let (rc, p) = unsafe { PointBox::from_c_out(|_| 7) };
    assert_eq!((rc, p.is_none()), (7, true));
}

#[test]
fn string_out_parameters_are_adopted() {
    /// Stands in for `void get_name(char **out)`.
    unsafe fn get_name(out: *mut *mut c_char) {
        let s = CStrBox::<RustAlloc>::from_c_str(c"pt").unwrap().into_raw();
        // SAFETY: the caller passes a writable slot.
        unsafe { *out = s };
    }
    // SAFETY: `get_name` stores an owned `RustAlloc` string.
    let ((), s) = unsafe { CStrBox::<RustAlloc>::from_raw_out(|out| get_name(out)) };
    assert_eq!(s.unwrap().as_c_str(), c"pt");
}

// The exports: what a ported module offers the C that still allocates,
// copies and frees its objects.
mod ffi_export {
    use super::*;

    /// Deep copies through `RustAlloc`, to exercise `export_cdupclone!`.
    #[derive(Clone, Copy, Debug, Default)]
    pub struct PointCopy;
    // SAFETY: delegates to `RustAlloc`.
    unsafe impl CDrop<Point> for PointCopy {
        unsafe fn c_drop(&self, ptr: NonNull<Point>) {
            // SAFETY: as the caller's.
            unsafe { CDrop::<Point>::c_drop(&RustAlloc, ptr) }
        }
    }
    // SAFETY: a fresh `RustAlloc` point holding a copy of the plain fields.
    unsafe impl CDupClone<Point> for PointCopy {
        unsafe fn c_dup(&self, ptr: NonNull<Point>) -> Option<NonNull<Point>> {
            let copy = PointBox::new()?.into_raw();
            // SAFETY: both are live points; the fields are plain data.
            unsafe { core::ptr::copy_nonoverlapping(ptr.as_ptr(), copy, 1) };
            NonNull::new(copy)
        }
    }

    ffibox::export_cnew!(pub test_point_new, Point, RustAlloc);
    ffibox::export_cdupclone!(pub test_point_dup, Point, PointCopy);
    ffibox::export_cdrop!(pub test_point_free, Point, RustAlloc);
}

#[test]
fn exports_offer_the_policy_to_c() {
    use ffi_export::*;
    let mut p = test_point_new().unwrap();
    p.as_mut().set_x(3);
    let q = test_point_dup(Some(p.as_ref())).unwrap();
    assert_eq!(q.as_ref().x(), 3);
    assert_ne!(q.as_ptr(), p.as_ptr());
    assert!(test_point_dup(None).is_none());
    test_point_free(Some(p));
    test_point_free(None);
    // The copy is owned under `PointCopy`, so Rust will not pass it to
    // `test_point_free`, which takes a `RustAlloc` box; it drops under its own
    // policy. C, holding plain pointers, frees it with `test_point_free`,
    // which `PointCopy` allows by freeing through `RustAlloc`.
    drop(q);
}

/// What C calls: each export through the signature C declares, with plain
/// `point_st *` arguments and results.
#[test]
fn exports_are_called_through_the_c_signatures() {
    // As C declares them.
    type New = unsafe extern "C" fn() -> *mut point_st;
    type Dup = unsafe extern "C" fn(*const point_st) -> *mut point_st;
    type Free = unsafe extern "C" fn(*mut point_st);
    // As Rust defines them.
    type RustNew = extern "C" fn() -> Option<PointBox>;
    type RustDup =
        for<'a> extern "C" fn(Option<PointRef<'a>>) -> Option<CBox<Point, ffi_export::PointCopy>>;
    type RustFree = extern "C" fn(Option<PointBox>);
    // SAFETY: an `Option` of an owner or a handle is `#[repr(transparent)]`
    // over a `NonNull`, with `None` as NULL, which Rust makes ABI-compatible
    // with the raw pointer; Miri checks every call below.
    let (new, dup, free) = unsafe {
        (
            core::mem::transmute::<RustNew, New>(ffi_export::test_point_new),
            core::mem::transmute::<RustDup, Dup>(ffi_export::test_point_dup),
            core::mem::transmute::<RustFree, Free>(ffi_export::test_point_free),
        )
    };
    // SAFETY: C's side of the protocol: each object is live until freed, and
    // freed once; NULL is accepted everywhere.
    unsafe {
        let p = new();
        assert!(!p.is_null());
        (*p).x = 3;
        let q = dup(p);
        assert_eq!(((*q).x, (*q).y), (3, 0));
        assert!(dup(null_mut()).is_null());
        free(p);
        free(q);
        free(null_mut());
        // An object C allocated is adopted back by the policy that frees it.
        let b = PointBox::from_c(new()).unwrap();
        assert_eq!(b.as_ref().x(), 0);
    }
}
