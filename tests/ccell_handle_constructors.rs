#![allow(missing_docs, clippy::undocumented_unsafe_blocks, clippy::missing_safety_doc)]
//! `CCell`'s handle constructors: generic code over `T: CCell` borrows a
//! pointer C hands it (typed, or erased to `void *`) as `T::Ref` / `T::Mut`,
//! and the handles `define_ctype!` generates forward their `from_ptr` there.

use core::ffi::c_void;
use core::ptr::{addr_of, addr_of_mut};

use ffibox::{define_ctype, CCell};

#[repr(C)]
pub struct pt_st {
    x: i32,
    y: i32,
}
define_ctype!(Pt, PtRef, PtMut, pt_st);

impl PtRef<'_> {
    fn x(&self) -> i32 {
        unsafe { addr_of!((*self.as_ptr()).x).read() }
    }
}
impl PtMut<'_> {
    fn set_x(&mut self, v: i32) {
        unsafe { addr_of_mut!((*self.as_mut_ptr()).x).write(v) }
    }
}

/// A trampoline in the shape C callbacks have: the object arrives as a
/// `void *`, and generic code turns it into the wrapper's exclusive handle.
unsafe fn visit<T: CCell + 'static>(arg: *mut c_void, f: impl FnOnce(T::Mut<'_>)) -> bool {
    match unsafe { T::mut_from_void_ptr(arg) } {
        Some(m) => {
            f(m);
            true
        }
        None => false,
    }
}

#[test]
fn generic_code_borrows_an_erased_pointer() {
    let mut raw = pt_st { x: 1, y: 2 };
    let arg = (&raw mut raw).cast::<c_void>();
    assert!(unsafe { visit::<Pt>(arg, |mut m| m.set_x(7)) });
    let r = unsafe { <Pt as CCell>::ref_from_void_ptr(arg) }.unwrap();
    assert_eq!(r.x(), 7);
    assert_eq!(raw.y, 2);
}

#[test]
fn typed_pointers_and_null() {
    let mut raw = pt_st { x: 3, y: 4 };
    let p = &raw mut raw;
    let mut m = unsafe { Pt::mut_from_ptr(p) }.unwrap();
    m.set_x(5);
    assert_eq!(m.as_ref().x(), 5);
    assert!(unsafe { Pt::ref_from_ptr(core::ptr::null_mut()) }.is_none());
    assert!(unsafe { Pt::mut_from_void_ptr(core::ptr::null_mut()) }.is_none());
}

#[test]
fn inherent_from_ptr_forwards_to_the_trait() {
    let mut raw = pt_st { x: 9, y: 0 };
    let p = &raw mut raw;
    let inherent = unsafe { PtRef::from_ptr(p) }.unwrap();
    let generic = unsafe { Pt::ref_from_ptr(p) }.unwrap();
    assert_eq!(inherent.as_ptr(), generic.as_ptr());
    let erased = unsafe { PtRef::from_void_ptr(p.cast()) }.unwrap();
    assert_eq!(erased.as_ptr(), generic.as_ptr());
    assert!(unsafe { PtRef::from_ptr(core::ptr::null_mut()) }.is_none());
    assert!(unsafe { PtMut::from_ptr(core::ptr::null_mut()) }.is_none());
}
