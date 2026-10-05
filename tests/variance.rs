//! Variance of the owners, guards and handles over a lifetime-generic layout
//! type. The exclusive paths — `Mut` handles, `CGuard`, `CGuardedRef` — must
//! be invariant in the pointee (the `compile_fail` doctests on `CCell`,
//! `CGuard` and `CGuardedRef` check that); the shared and sole-owner paths
//! stay covariant, like `&T`, `Arc<T>` and `Box<T>`. A `CArc` that can lock
//! is kept from shrinking by its pointee instead, which `CGuarded`'s contract
//! makes invariant, as a `Mutex` field makes a Rust type. This file mirrors
//! the doctests' setup, so it also proves their failures come from variance
//! rather than from a broken setup.

#![allow(non_camel_case_types, missing_docs, dead_code)]

use core::marker::PhantomData;
use core::ptr::NonNull;

use ffibox::{
    CArc, CBorrowedPtr, CBox, CCell, CDrop, CGuard, CGuarded, CGuardedAll, CGuardedRef, CRefClone,
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

/// The same C struct with a lock: invariant in `'x`, as `CGuarded` requires,
/// since a locked handle may store a `&'x u32` that other holders read.
#[repr(transparent)]
pub struct Locked<'x>(holder_st, PhantomData<fn(&'x u32) -> &'x u32>);
#[repr(transparent)]
#[derive(Clone, Copy)]
pub struct LockedRef<'a, 'x>(CBorrowedPtr<'a, Locked<'x>>);
#[repr(transparent)]
pub struct LockedMut<'a, 'x>(LockedRef<'a, 'x>, PhantomData<&'a mut Locked<'x>>);

// SAFETY: as for `Holder`.
unsafe impl<'x> CCell for Locked<'x> {
    type C = holder_st;
    type Ref<'a>
        = LockedRef<'a, 'x>
    where
        Self: 'a;
    type Mut<'a>
        = LockedMut<'a, 'x>
    where
        Self: 'a;
}

// SAFETY: a mock that never touches the object; nothing is ever locked here.
// `Locked<'x>` is invariant, and its locked handle is the invariant `Mut`.
unsafe impl<'x> CGuarded for Locked<'x> {
    type Locked<'a>
        = LockedMut<'a, 'x>
    where
        Self: 'a;
    type Scope = ffibox::LockWhole;
    unsafe fn c_lock(_: NonNull<Self>) -> bool {
        true
    }
    unsafe fn c_unlock(_: NonNull<Self>) {}
}
// SAFETY: as above.
unsafe impl<'x> CGuardedAll for Locked<'x> {}

// SAFETY: never invoked, as for `Holder`.
unsafe impl<'x> CDrop<Locked<'x>> for Unref {
    unsafe fn c_drop(&self, _: NonNull<Locked<'x>>) {}
}

// Exclusive paths: the identity compiles, so the doctests' shrinking versions
// fail only on variance.
fn keep_mut<'a>(m: HolderMut<'a, 'static>) -> HolderMut<'a, 'static> {
    m
}
fn keep_guard<'g>(g: CGuard<'g, Locked<'static>>) -> CGuard<'g, Locked<'static>> {
    g
}
fn keep_locked_arc(a: CArc<Locked<'static>, Unref>) -> CArc<Locked<'static>, Unref> {
    a
}
fn keep_guarded_ref<'a>(r: CGuardedRef<'a, Locked<'static>>) -> CGuardedRef<'a, Locked<'static>> {
    r
} // The borrow itself still shortens, as `&'a Mutex<T>` does; only `T` is fixed.
fn shorten_guarded_ref<'a>(
    r: CGuardedRef<'static, Locked<'static>>,
) -> CGuardedRef<'a, Locked<'static>> {
    r
}

// Shared and sole-owner paths stay covariant.
fn shrink_ref<'a, 's>(r: HolderRef<'a, 'static>) -> HolderRef<'a, 's> {
    r
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
