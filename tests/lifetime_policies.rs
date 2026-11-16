//! The policy macros over a hand-written layout type that carries a lifetime.
//!
//! `define_ctype!` declares such a type too (`tests/lifetime_ctypes.rs`); one
//! written by hand against `CCell`, as `manual_wrappers.rs` does, takes the
//! same policies. `impl_cdrop!`, `impl_crefclone!` and `impl_cdispose!` take
//! the type with its lifetime spelled `'_`, which the impl header turns into a
//! parameter, so one policy serves `Holder<'r>` for every `'r`. These tests
//! bind each of them so, and drive the owners over a holder borrowing a local,
//! so a non-`'static` lifetime.
//!
//! `impl_cdupclone!` names the lifetime instead, in a leading `<'p>` list: its
//! `c_dup` returns the type, and a `'_` in return position elides to `&self`'s
//! lifetime, so the signature would no longer match the trait.
//! `impl_cdupclone!`'s rustdoc carries the `compile_fail` that shows the
//! elided form's refusal.

#![allow(non_camel_case_types, missing_docs)]

use core::ffi::c_int;
use core::marker::PhantomData;
use core::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};

use ffibox::{
    CArc, CBorrowedPtr, CBox, CCell, CRefSeam, CVal, impl_cdispose, impl_cdrop, impl_cdupclone,
    impl_crefclone,
};

// ---------------------------------------------------------------------------
// A C object that keeps a pointer to its parent, as `git_blob` keeps its
// repository: the Rust layout type carries the parent's lifetime.
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct holder_st {
    rc: usize,
    parent: *const u32,
}

/// Lives no longer than the parent `'p` its C struct points at.
#[repr(transparent)]
pub struct Holder<'p>(holder_st, PhantomData<(*const (), &'p u32)>);

#[repr(transparent)]
pub struct HolderRef<'a, 'p>(CBorrowedPtr<'a, Holder<'p>>);
impl Clone for HolderRef<'_, '_> {
    fn clone(&self) -> Self {
        *self
    }
}
impl Copy for HolderRef<'_, '_> {}

#[repr(transparent)]
pub struct HolderMut<'a, 'p>(HolderRef<'a, 'p>, PhantomData<&'a mut Holder<'p>>);

// SAFETY: `Holder` is transparent over `holder_st`, its other field zero-sized;
// both handles are transparent over `CBorrowedPtr<'a, Holder<'p>>` (`Mut`
// through `Ref`), have no `Drop`, hand out no reference to `Holder`, and `Mut`
// is invariant in `Holder<'p>` through its marker. `Ref` writes nothing.
unsafe impl<'p> CCell for Holder<'p> {
    type C = holder_st;
    type Ref<'a>
        = HolderRef<'a, 'p>
    where
        Self: 'a;
    type Mut<'a>
        = HolderMut<'a, 'p>
    where
        Self: 'a;
}

impl<'a, 'p> CRefSeam<'a> for HolderRef<'a, 'p> {
    type Cell = Holder<'p>;
}

impl HolderRef<'_, '_> {
    /// The parent's value, read through the pointer the C struct keeps.
    fn parent(&self) -> u32 {
        // SAFETY: the handle addresses a live holder, whose parent outlives it
        // by `Holder`'s lifetime; reads only, through raw pointers.
        unsafe { *ptr::addr_of!((*self.as_ptr()).parent).read() }
    }
}

impl<'p> Holder<'p> {
    /// A holder of `parent`, on the heap, with one reference.
    fn alloc(parent: &'p u32) -> *mut holder_st {
        Box::into_raw(Box::new(holder_st {
            rc: 1,
            parent: parent as *const u32,
        }))
    }

    /// The same, held inline.
    fn inline(parent: &'p u32) -> Self {
        Holder(
            holder_st {
                rc: 1,
                parent: parent as *const u32,
            },
            PhantomData,
        )
    }
}

// ---------------------------------------------------------------------------
// Mock C routines
// ---------------------------------------------------------------------------

static FREES: AtomicUsize = AtomicUsize::new(0);
static UP_REFS: AtomicUsize = AtomicUsize::new(0);
static DISPOSES: AtomicUsize = AtomicUsize::new(0);
static DUPS: AtomicUsize = AtomicUsize::new(0);

/// Serialises the tests, which count through the statics above.
static LOCK: Mutex<()> = Mutex::new(());

fn lock() -> MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// # Safety
///
/// `p` is a live `holder_alloc` holder whose reference the caller gives up.
unsafe fn holder_free(p: *mut holder_st) {
    FREES.fetch_add(1, Ordering::SeqCst);
    // SAFETY: the caller gives up its reference to a live holder.
    let rc = unsafe { ptr::addr_of_mut!((*p).rc) };
    // SAFETY: as above; the count is read and written through the raw pointer.
    unsafe { *rc -= 1 };
    // SAFETY: as above.
    if unsafe { *rc } == 0 {
        // SAFETY: the last reference: the `Box` allocation is ours to free.
        drop(unsafe { Box::from_raw(p) });
    }
}

/// An up_ref through an out-parameter, shaped as `git_blob_dup` is: it stores
/// the same pointer in `out` and returns 0.
///
/// # Safety
///
/// `out` is writable; `src` is a live holder whose reference the caller holds.
unsafe fn holder_dup_slot(out: *mut *mut holder_st, src: *mut holder_st) -> c_int {
    UP_REFS.fetch_add(1, Ordering::SeqCst);
    // SAFETY: the caller holds a reference to the live holder.
    unsafe { (*src).rc += 1 };
    // SAFETY: `out` is writable.
    unsafe { out.write(src) };
    0
}

/// # Safety
///
/// `p` is a live holder; the count is read without a race (one thread here).
unsafe fn holder_is_sole(p: *mut holder_st) -> bool {
    // SAFETY: the caller's reference keeps the holder live.
    unsafe { (*p).rc == 1 }
}

/// Disposes an inline holder's resources: it owns none, so it only counts.
///
/// # Safety
///
/// `p` is a live holder.
unsafe fn holder_dispose(p: *mut holder_st) {
    DISPOSES.fetch_add(1, Ordering::SeqCst);
    // SAFETY: the caller hands a live holder; nothing reads it afterwards.
    unsafe { (*p).parent = ptr::null() };
}

// ---------------------------------------------------------------------------
// The policies: each macro takes `Holder<'_>`, one impl for every lifetime.
// ---------------------------------------------------------------------------

/// The sole owner's free.
#[derive(Clone, Copy, Debug, Default)]
pub struct HolderFree;
impl_cdrop!(HolderFree, Holder<'_>, holder_free);

/// The down-ref and the up_ref of a counted holder. The up_ref takes its
/// result through an out-parameter, so a private adapter gives it the shape
/// the macro calls, as a destructor of another shape gets one.
#[derive(Clone, Copy, Debug, Default)]
pub struct HolderUnref;

/// # Safety
///
/// `p` is a live holder whose reference the caller holds.
unsafe fn holder_up_ref(p: *mut holder_st) -> c_int {
    let mut out = ptr::null_mut();
    // SAFETY: `out` is a local slot; the caller's contract covers `p`.
    let rc = unsafe { holder_dup_slot(&raw mut out, p) };
    debug_assert_eq!(out, p);
    rc
}

impl_cdrop!(HolderUnref, Holder<'_>, holder_free);
impl_crefclone!(
    HolderUnref,
    Holder<'_>,
    holder_up_ref,
    ok = |r| r == 0,
    sole = holder_is_sole
);

/// The inline value's disposal.
#[derive(Clone, Copy, Debug, Default)]
pub struct HolderDispose;
impl_cdispose!(HolderDispose, Holder<'_>, holder_dispose);

/// A deep copy over a lifetime-carrying type, the lifetime named in the
/// macro's leading list.
#[derive(Clone, Copy, Debug, Default)]
pub struct HolderCopy;
impl_cdrop!(HolderCopy, Holder<'_>, holder_free);
impl_cdupclone!(<'p> HolderCopy, Holder<'p>, holder_copy);

/// A fresh holder of the same parent, with its own count of 1.
///
/// # Safety
///
/// `src` is a live holder, which is only read.
unsafe fn holder_copy(src: *mut holder_st) -> *mut holder_st {
    DUPS.fetch_add(1, Ordering::SeqCst);
    // SAFETY: the caller hands a live holder.
    let parent = unsafe { (*src).parent };
    Box::into_raw(Box::new(holder_st { rc: 1, parent }))
}

fn reset() {
    for c in [&FREES, &UP_REFS, &DISPOSES, &DUPS] {
        c.store(0, Ordering::SeqCst);
    }
}

// ---------------------------------------------------------------------------
// The owners over a holder of a local: a lifetime shorter than `'static`.
// ---------------------------------------------------------------------------

#[test]
fn impl_cdrop_frees_a_box_of_a_lifetime_carrying_type() {
    let _g = lock();
    reset();
    let parent = 7_u32;
    {
        // SAFETY: a fresh holder, with the one reference this box takes.
        let b = unsafe { CBox::<Holder<'_>, HolderFree>::from_c(Holder::alloc(&parent)) }.unwrap();
        assert_eq!(b.as_ref().parent(), 7);
    }
    assert_eq!(FREES.load(Ordering::SeqCst), 1);
}

#[test]
fn impl_crefclone_counts_an_arc_of_a_lifetime_carrying_type() {
    let _g = lock();
    reset();
    let parent = 9_u32;
    {
        // SAFETY: a fresh holder, with the one reference this arc takes.
        let mut a =
            unsafe { CArc::<Holder<'_>, HolderUnref>::from_c(Holder::alloc(&parent)) }.unwrap();
        // `sole` is bound: the only reference reaches the exclusive handle.
        assert!(a.get_mut().is_some());

        let b = a.clone();
        assert_eq!(UP_REFS.load(Ordering::SeqCst), 1);
        assert!(CArc::ptr_eq(&a, &b));
        assert!(a.get_mut().is_none());
        assert_eq!(b.as_ref().parent(), 9);

        drop(b);
        assert_eq!(FREES.load(Ordering::SeqCst), 1);
        assert!(a.get_mut().is_some());
    }
    assert_eq!(FREES.load(Ordering::SeqCst), 2);
}

#[test]
fn impl_cdispose_disposes_an_inline_lifetime_carrying_value() {
    let _g = lock();
    reset();
    let parent = 3_u32;
    {
        // SAFETY: the value owns nothing anyone else holds.
        let v = unsafe { CVal::<Holder<'_>, HolderDispose>::new(Holder::inline(&parent)) };
        assert_eq!(v.as_ref().parent(), 3);
    }
    assert_eq!(DISPOSES.load(Ordering::SeqCst), 1);
}

#[test]
fn impl_cdupclone_clones_a_box_of_a_lifetime_carrying_type() {
    let _g = lock();
    reset();
    let parent = 5_u32;
    {
        // SAFETY: a fresh holder, with the one reference this box takes.
        let b = unsafe { CBox::<Holder<'_>, HolderCopy>::from_c(Holder::alloc(&parent)) }.unwrap();
        let c = b.clone();
        assert_eq!(DUPS.load(Ordering::SeqCst), 1);
        assert_ne!(b.as_ptr(), c.as_ptr());
        assert_eq!(c.as_ref().parent(), 5);
    }
    assert_eq!(FREES.load(Ordering::SeqCst), 2);
}
