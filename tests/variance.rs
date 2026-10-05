//! Variance of the owners, guards and handles over a lifetime-generic layout
//! type. The exclusive paths — `Mut` handles, `CWriteGuard`, `CGuardedArc` —
//! must be invariant in the pointee (the `compile_fail` doctests on `CCell`,
//! `CWriteGuard` and `CGuardedArc` check that); the shared and sole-owner
//! paths stay covariant, like `&T`, `Arc<T>` and `Box<T>`. This file mirrors
//! the doctests' setup, so it also proves their failures come from variance
//! rather than from a broken setup.

#![allow(non_camel_case_types, missing_docs, dead_code)]

use core::marker::PhantomData;
use core::ptr::NonNull;

use ffibox::{
    CArc, CBorrowedPtr, CBox, CCell, CDrop, CGuarded, CGuardedArc, CGuardedRef, CReadGuard,
    CRefClone, CWriteGuard,
};

#[repr(C)]
pub struct holder_st {
    p: *const u32,
}
/// Holds a `&'x u32` in a C field.
#[repr(transparent)]
pub struct Holder<'x>(holder_st, PhantomData<&'x u32>);
#[repr(transparent)]
#[derive(Clone, Copy)]
pub struct HolderRef<'a, 'x>(CBorrowedPtr<'a, Holder<'x>>);
#[repr(transparent)]
pub struct HolderMut<'a, 'x>(HolderRef<'a, 'x>, PhantomData<&'a mut Holder<'x>>);

// SAFETY: `Holder` is transparent over `holder_st`; the handles are
// transparent over `CBorrowedPtr`, `HolderMut` invariant in `Holder<'x>`.
unsafe impl<'x> CCell for Holder<'x> {
    type C = holder_st;
    type Ref<'a>
        = HolderRef<'a, 'x>
    where
        Self: 'a;
    type Mut<'a>
        = HolderMut<'a, 'x>
    where
        Self: 'a;
}

// SAFETY: a mock that never touches the object; nothing is ever locked here.
unsafe impl<'x> CGuarded for Holder<'x> {
    unsafe fn c_lock(_: NonNull<Self>) -> bool {
        true
    }
    unsafe fn c_unlock(_: NonNull<Self>) {}
}

#[derive(Clone)]
pub struct Unref;
// SAFETY: never invoked; the functions below only move values around.
unsafe impl<'x> CDrop<Holder<'x>> for Unref {
    unsafe fn c_drop(&self, _: NonNull<Holder<'x>>) {}
}
// SAFETY: as above.
unsafe impl<'x> CRefClone<Holder<'x>> for Unref {
    unsafe fn c_up_ref(&self, _: NonNull<Holder<'x>>) -> bool {
        true
    }
}

// Exclusive paths: the identity compiles, so the doctests' shrinking versions
// fail only on variance.
fn keep_mut<'a>(m: HolderMut<'a, 'static>) -> HolderMut<'a, 'static> {
    m
}
fn keep_write<'g>(g: CWriteGuard<'g, Holder<'static>>) -> CWriteGuard<'g, Holder<'static>> {
    g
}
fn keep_guarded(a: CGuardedArc<Holder<'static>, Unref>) -> CGuardedArc<Holder<'static>, Unref> {
    a
}
fn keep_guarded_ref<'a>(r: CGuardedRef<'a, Holder<'static>>) -> CGuardedRef<'a, Holder<'static>> {
    r
} // The borrow itself still shortens, as `&'a RwLock<T>` does; only `T` is fixed.
fn shorten_guarded_ref<'a>(
    r: CGuardedRef<'static, Holder<'static>>,
) -> CGuardedRef<'a, Holder<'static>> {
    r
}

// Shared and sole-owner paths stay covariant.
fn shrink_ref<'a, 's>(r: HolderRef<'a, 'static>) -> HolderRef<'a, 's> {
    r
}
fn shrink_read<'g, 's>(g: CReadGuard<'g, Holder<'static>>) -> CReadGuard<'g, Holder<'s>> {
    g
}
fn shrink_arc<'s>(a: CArc<Holder<'static>, Unref>) -> CArc<Holder<'s>, Unref> {
    a
}
fn shrink_box<'s>(b: CBox<Holder<'static>, Unref>) -> CBox<Holder<'s>, Unref> {
    b
}

#[test]
fn the_setup_shared_with_the_variance_doctests_compiles() {
    // The checks above are compile-time; reaching here means they all held.
}
