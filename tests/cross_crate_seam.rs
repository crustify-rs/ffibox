#![allow(missing_docs)]
//! The handles' seam from a crate that did not invoke `define_ctype!`, through
//! `CRefSeam` / `CMutSeam` / `CCell` as the owning crate uses them.

use ffibox::{CCell, CMutSeam, CRefSeam, CZeroable};
use ffibox_seam_upstream::{widget_st, Widget, WidgetMut, WidgetRef};

#[test]
fn shared_handle_pointers_address_the_object() {
    let mut w = Widget::zeroed();
    w.as_mut().set_size(7);
    let r = w.as_ref();
    let p: *const widget_st = r.as_ptr();
    // SAFETY: `p` addresses `w`, live and unwritten for this read.
    assert_eq!(unsafe { (*p).size }, 7);
    assert_eq!(r.as_void_ptr(), p.cast());
    assert_eq!(p, core::ptr::addr_of!(w).cast());
}

#[test]
fn exclusive_handle_pointers_carry_write_provenance() {
    let mut w = Widget::zeroed();
    let mut m = w.as_mut();
    let p: *mut widget_st = m.as_mut_ptr();
    // SAFETY: `p` comes from the only handle to `w`, which is live.
    unsafe { (*p).size = 9 };
    assert_eq!(m.as_mut_void_ptr(), p.cast());
    assert_eq!(w.as_ref().size(), 9);
}

#[test]
fn inbound_seam_on_the_handles_and_the_newtype() {
    let mut w = Widget::zeroed();
    w.as_mut().set_size(3);
    let p = w.as_mut().as_mut_ptr();

    // SAFETY: `p` addresses `w`, live for every handle below, used one at a time.
    let r = unsafe { WidgetRef::from_ptr(p) }.unwrap();
    assert_eq!(r.size(), 3);
    // SAFETY: as above, erased from this very type.
    let r = unsafe { WidgetRef::from_void_ptr(p.cast()) }.unwrap();
    assert_eq!(r.size(), 3);
    // SAFETY: as above; no other handle is used while `m` lives.
    let mut m = unsafe { WidgetMut::from_ptr(p) }.unwrap();
    m.set_size(4);
    // SAFETY: as above.
    let mut m = unsafe { WidgetMut::from_void_ptr(p.cast()) }.unwrap();
    m.set_size(5);
    // SAFETY: as above, through the newtype's `CCell` seam.
    let r = unsafe { Widget::ref_from_ptr(p) }.unwrap();
    assert_eq!(r.size(), 5);

    // SAFETY: null is allowed and yields `None`.
    assert!(unsafe { WidgetRef::from_ptr(core::ptr::null_mut()) }.is_none());
    // SAFETY: as above.
    assert!(unsafe { WidgetMut::from_void_ptr(core::ptr::null_mut()) }.is_none());
}

/// Generic code over any wrapped type reaches the seam through the traits.
fn c_address<'a, R: CRefSeam<'a>>(r: R) -> usize {
    r.as_ptr() as usize
}

fn c_mut_address<'a, M: CMutSeam<'a>>(mut m: M) -> usize {
    m.as_mut_ptr() as usize
}

#[test]
fn generic_code_over_the_seam() {
    let mut w = Widget::zeroed();
    let at = core::ptr::addr_of!(w) as usize;
    assert_eq!(c_address(w.as_ref()), at);
    assert_eq!(c_mut_address(w.as_mut()), at);
}
