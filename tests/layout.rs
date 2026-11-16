//! The layout each owner and handle promises the foreign side: with a
//! stateless policy, an owner is the pointer it stands in for, and `None` is
//! NULL; a by-value owner is its value's size; a buffer is C's
//! `{ T *ptr; size_t len; }`.

#![allow(missing_docs)]

use core::ffi::c_char;
use core::mem::{align_of, size_of};
use core::ptr::NonNull;

use ffibox::{
    CArc, CBorrowedPtr, CBox, CBoxState, CDispose, CDrop, CSlice, CStrBox, CVal, CVec, RustAlloc,
    define_ctype,
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
    // A stateful policy travels with the pointer, so its owner widens.
    assert!(size_of::<CBoxState<Obj, Stateful>>() == 2 * PTR);
};

#[test]
fn a_none_owner_is_a_null_pointer() {
    let none: Option<CBox<Obj, Stateless>> = None;
    // SAFETY: `Option<CBox<_, ZST>>` is a nullable pointer (asserted above),
    // and reading its bytes as one reads no padding.
    let raw: *mut obj_st = unsafe { core::mem::transmute(none) };
    assert!(raw.is_null());
}

/// The owners and handles are FFI-safe in an `extern "C"` signature: the
/// lint that rejects a type Rust does not lay out as C's is denied here.
#[deny(improper_ctypes_definitions)]
mod ffi_safe {
    use super::*;

    pub extern "C" fn owners(
        _: Option<CBox<Obj, Stateless>>,
        _: Option<CArc<Obj, Stateless>>,
        _: Option<CStrBox<RustAlloc>>,
        _: Option<ObjRef<'_>>,
        _: Option<ObjMut<'_>>,
    ) -> Option<CBox<Obj, Stateless>> {
        None
    }
}

#[test]
fn owners_cross_extern_c_by_value() {
    assert!(ffi_safe::owners(None, None, None, None, None).is_none());
}
