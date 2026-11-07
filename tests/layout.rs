//! The layout each owner and handle promises the foreign side: with a
//! stateless policy, an owner is the pointer it stands in for, and `None` is
//! NULL; a by-value owner is its value's size; a buffer is C's
//! `{ T *ptr; size_t len; }`.

#![allow(missing_docs)]

use core::ffi::c_char;
use core::mem::{align_of, size_of};
use core::ptr::NonNull;

use ffibox::{
    CArc, CBorrowedPtr, CBox, CDispose, CDrop, CSlice, CStrBox, CVal, CVec, RustAlloc, define_ctype,
};

#[repr(C)]
pub struct obj_st {
    a: u64,
    b: u32,
}
define_ctype!(Obj, ObjRef, ObjMut, obj_st);

#[derive(Clone, Copy, Default)]
struct Stateless;
// SAFETY: never called; layout only.
unsafe impl CDrop<Obj> for Stateless {
    unsafe fn c_drop(&self, _: NonNull<Obj>) {}
}
// SAFETY: never called; layout only.
unsafe impl CDispose<Obj> for Stateless {
    unsafe fn c_dispose(&self, _: NonNull<Obj>) {}
}

/// A policy carrying runtime state, which makes an owner wider.
#[derive(Clone, Copy)]
struct Stateful(#[allow(dead_code)] usize);
// SAFETY: never called; layout only.
unsafe impl CDrop<Obj> for Stateful {
    unsafe fn c_drop(&self, _: NonNull<Obj>) {}
}

const PTR: usize = size_of::<*mut obj_st>();

// Checked at compile time: a change to any of these fails the build.
const _: () = {
    assert!(size_of::<CBox<Obj, Stateless>>() == PTR);
    assert!(size_of::<Option<CBox<Obj, Stateless>>>() == PTR);
    assert!(align_of::<CBox<Obj, Stateless>>() == align_of::<*mut obj_st>());
    assert!(size_of::<CArc<Obj, Stateless>>() == PTR);
    assert!(size_of::<Option<CArc<Obj, Stateless>>>() == PTR);
    assert!(size_of::<CStrBox<RustAlloc>>() == size_of::<*mut c_char>());
    assert!(size_of::<Option<CStrBox<RustAlloc>>>() == size_of::<*mut c_char>());
    assert!(size_of::<CVal<Obj, Stateless>>() == size_of::<obj_st>());
    assert!(align_of::<CVal<Obj, Stateless>>() == align_of::<obj_st>());
    assert!(size_of::<Obj>() == size_of::<obj_st>());
    assert!(size_of::<CVec<u32, RustAlloc>>() == 2 * PTR);
    assert!(size_of::<ObjRef<'static>>() == PTR);
    assert!(size_of::<Option<ObjRef<'static>>>() == PTR);
    assert!(size_of::<ObjMut<'static>>() == PTR);
    assert!(size_of::<Option<CBorrowedPtr<'static, Obj>>>() == PTR);
    assert!(size_of::<CSlice<'static, Obj>>() == 2 * PTR);
    // A stateful policy travels with the pointer, so the owner widens.
    assert!(size_of::<CBox<Obj, Stateful>>() == 2 * PTR);
};

#[test]
fn a_none_owner_is_a_null_pointer() {
    let none: Option<CBox<Obj, Stateless>> = None;
    // SAFETY: `Option<CBox<_, ZST>>` is a nullable pointer (asserted above),
    // and reading its bytes as one reads no padding.
    let raw: *mut obj_st = unsafe { core::mem::transmute(none) };
    assert!(raw.is_null());
}
