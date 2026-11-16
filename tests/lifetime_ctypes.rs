//! `define_ctype!` over a layout type that carries a lifetime.
//!
//! The macro's lifetime form, `Holder<'p>, …, phantom = &'p u32`, mints the
//! same triple a wrapper writes by hand against `CCell`: these tests build one
//! of each over the same C struct and check that they agree, in layout and in
//! variance, and drive the macro's type through every owner, with policies
//! from the macros, `impl_cdupclone!` and `impl_cguarded!` taking the
//! lifetime in a leading `<'p>` list.
//!
//! The variance functions below are the passing halves of the `compile_fail`
//! doctests on `define_ctype!`: the identity of each exclusive handle
//! compiles, so a doctest that shrinks or widens one fails on variance alone,
//! not on a broken setup.

#![allow(non_camel_case_types, missing_docs, dead_code)]

use core::ffi::c_int;
use core::marker::PhantomData;
use core::mem::{align_of, size_of};
use core::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};

use ffibox::{
    CArc, CBorrowedPtr, CBox, CCell, CGuardedRef, CLockedSeam, CMutSeam, CRefSeam, CVal, CZeroable,
    define_ctype, impl_cdispose, impl_cdrop, impl_cdupclone, impl_cguarded, impl_crefclone,
};

// ---------------------------------------------------------------------------
// The C object: it keeps a pointer to a parent that must outlive it, as
// `git_reference` keeps its repository.
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct holder_st {
    rc: usize,
    locked: bool,
    parent: *const u32,
}

define_ctype!(
    /// Lives no longer than the parent `'p` its C struct points at.
    Holder<'p>,
    HolderRef,
    HolderMut,
    holder_st,
    phantom = &'p u32
);

// The same triple by hand, as a campaign writes it (`Reference<'r>`).
#[repr(transparent)]
pub struct Manual<'p>(holder_st, PhantomData<(*const (), &'p u32)>);
#[repr(transparent)]
#[derive(Clone, Copy)]
pub struct ManualRef<'a, 'p>(CBorrowedPtr<'a, Manual<'p>>);
#[repr(transparent)]
pub struct ManualMut<'a, 'p>(ManualRef<'a, 'p>, PhantomData<&'a mut Manual<'p>>);
// SAFETY: `Manual` is transparent over `holder_st` with a zero-sized marker;
// both handles are transparent over `CBorrowedPtr<'a, Manual<'p>>` (`Mut`
// through `Ref`) with no `Drop`, and `ManualMut` is invariant through its
// marker. Nothing here reads or writes through them.
unsafe impl<'p> CCell for Manual<'p> {
    type C = holder_st;
    type Ref<'a>
        = ManualRef<'a, 'p>
    where
        Self: 'a;
    type Mut<'a>
        = ManualMut<'a, 'p>
    where
        Self: 'a;
}
impl<'a, 'p: 'a> CRefSeam<'a> for ManualRef<'a, 'p> {
    type Cell = Manual<'p>;
}
impl<'a, 'p: 'a> CMutSeam<'a> for ManualMut<'a, 'p> {
    type Cell = Manual<'p>;
}

// A layout type invariant in its lifetime, as `CGuarded` requires of one whose
// locked state can hold a `&'x u32`: the marker is the caller's.
define_ctype!(Guarded<'x>, GuardedRef, GuardedMut, holder_st, phantom = fn(&'x u32) -> &'x u32);

// One lock over the whole object, for `CGuardedRef`.
define_ctype!(Whole<'x>, WholeRef, WholeMut, holder_st, phantom = fn(&'x u32) -> &'x u32);

// Two lifetimes, both carried into the handles after the borrow's.
define_ctype!(Pair<'p, 'q>, PairRef, PairMut, holder_st, phantom = (&'p u32, &'q u32));

// ---------------------------------------------------------------------------
// Layout: the macro's handles are the hand-written ones' size, one pointer.
// ---------------------------------------------------------------------------

const PTR: usize = size_of::<*mut holder_st>();

const _: () = {
    assert!(size_of::<Holder<'static>>() == size_of::<holder_st>());
    assert!(align_of::<Holder<'static>>() == align_of::<holder_st>());
    assert!(size_of::<Holder<'static>>() == size_of::<Manual<'static>>());
    assert!(size_of::<HolderRef<'static, 'static>>() == PTR);
    assert!(size_of::<Option<HolderRef<'static, 'static>>>() == PTR);
    assert!(size_of::<HolderMut<'static, 'static>>() == PTR);
    assert!(size_of::<HolderRef<'static, 'static>>() == size_of::<ManualRef<'static, 'static>>());
    assert!(size_of::<HolderMut<'static, 'static>>() == size_of::<ManualMut<'static, 'static>>());
    assert!(size_of::<PairRef<'static, 'static, 'static>>() == PTR);
    assert!(size_of::<PairMut<'static, 'static, 'static>>() == PTR);
    assert!(size_of::<GuardedLocked<'static, 'static>>() == PTR);
    assert!(size_of::<CBox<Holder<'static>, HolderFree>>() == PTR);
    assert!(size_of::<Option<CBox<Holder<'static>, HolderFree>>>() == PTR);
    assert!(size_of::<CArc<Holder<'static>, HolderUnref>>() == PTR);
    assert!(size_of::<CVal<Holder<'static>, HolderDispose>>() == size_of::<holder_st>());
};

// ---------------------------------------------------------------------------
// Variance, side by side with the hand-written triple. Every function here
// compiles for both, or the handles would differ.
// ---------------------------------------------------------------------------

// The exclusive handles: the identity compiles; shrinking or widening `'p`
// does not (the `compile_fail` doctests on `define_ctype!`).
fn keep_mut<'a>(m: HolderMut<'a, 'static>) -> HolderMut<'a, 'static> {
    m
}
fn keep_manual_mut<'a>(m: ManualMut<'a, 'static>) -> ManualMut<'a, 'static> {
    m
}
fn keep_mut_short<'a, 's>(m: HolderMut<'a, 's>) -> HolderMut<'a, 's> {
    m
}
// The borrow itself still shortens, as `&'a mut T` does in `'a`.
fn shorten_mut_borrow<'a, 'p>(m: HolderMut<'static, 'p>) -> HolderMut<'a, 'p> {
    m
}
fn shorten_manual_mut_borrow<'a, 'p>(m: ManualMut<'static, 'p>) -> ManualMut<'a, 'p> {
    m
}

// The shared handles are covariant in both, as `&'a Holder<'p>` is.
fn shrink_ref<'a, 's>(r: HolderRef<'a, 'static>) -> HolderRef<'a, 's> {
    r
}
fn shrink_manual_ref<'a, 's>(r: ManualRef<'a, 'static>) -> ManualRef<'a, 's> {
    r
}
fn shorten_ref_borrow<'a, 'p>(r: HolderRef<'static, 'p>) -> HolderRef<'a, 'p> {
    r
}
fn shrink_pair<'a, 's, 't>(r: PairRef<'a, 'static, 'static>) -> PairRef<'a, 's, 't> {
    r
}

// The owners follow the layout type, which follows the caller's marker.
fn shrink_box<'s>(b: CBox<Holder<'static>, HolderFree>) -> CBox<Holder<'s>, HolderFree> {
    b
}
fn shrink_arc<'s>(a: CArc<Holder<'static>, HolderUnref>) -> CArc<Holder<'s>, HolderUnref> {
    a
}
// An invariant marker keeps even the shared handle's `'x` fixed.
fn keep_guarded_ref<'a>(r: GuardedRef<'a, 'static>) -> GuardedRef<'a, 'static> {
    r
}
fn keep_locked<'a>(l: GuardedLocked<'a, 'static>) -> GuardedLocked<'a, 'static> {
    l
}

// The `!Send` doctest's C type is `Send` alone, as a bindgen opaque type is,
// and so is its marker: only the macro's `*const ()` takes the trait away.
#[repr(C)]
pub struct opaque_st {
    _opaque: [u8; 0],
}
fn send<T: Send>() {}
fn sync<T: Sync>() {}
fn the_parts_are_send() {
    send::<opaque_st>();
    send::<&'static ()>();
    sync::<opaque_st>();
}

#[test]
fn the_variance_setup_shared_with_the_doctests_compiles() {
    // The checks above are compile-time; reaching here means they all held.
}

// ---------------------------------------------------------------------------
// Mock C routines
// ---------------------------------------------------------------------------

static FREES: AtomicUsize = AtomicUsize::new(0);
static UP_REFS: AtomicUsize = AtomicUsize::new(0);
static DUPS: AtomicUsize = AtomicUsize::new(0);
static DISPOSES: AtomicUsize = AtomicUsize::new(0);
static LOCKS: AtomicUsize = AtomicUsize::new(0);

/// Serialises the tests, which count through the statics above.
static LOCK: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn reset() {
    for c in [&FREES, &UP_REFS, &DUPS, &DISPOSES, &LOCKS] {
        c.store(0, Ordering::SeqCst);
    }
}

fn alloc(parent: &u32) -> *mut holder_st {
    Box::into_raw(Box::new(holder_st {
        rc: 1,
        locked: false,
        parent: parent as *const u32,
    }))
}

/// # Safety
///
/// `p` is a live `alloc` holder whose reference the caller gives up.
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

/// # Safety
///
/// `p` is a live holder whose reference the caller holds.
unsafe fn holder_up_ref(p: *mut holder_st) -> c_int {
    UP_REFS.fetch_add(1, Ordering::SeqCst);
    // SAFETY: the caller holds a reference to the live holder.
    unsafe { (*p).rc += 1 };
    0
}

/// # Safety
///
/// `p` is a live holder; one thread here, so the read does not race.
unsafe fn holder_is_sole(p: *mut holder_st) -> bool {
    // SAFETY: the caller's reference keeps the holder live.
    unsafe { (*p).rc == 1 }
}

/// A deep copy: a fresh holder of the same parent, with its own count.
///
/// # Safety
///
/// `p` is a live holder.
unsafe fn holder_dup(p: *mut holder_st) -> *mut holder_st {
    DUPS.fetch_add(1, Ordering::SeqCst);
    // SAFETY: the caller hands a live holder, which is only read.
    let parent = unsafe { (*p).parent };
    Box::into_raw(Box::new(holder_st {
        rc: 1,
        locked: false,
        parent,
    }))
}

/// # Safety
///
/// `p` is a live holder; nothing reads it afterwards.
unsafe fn holder_dispose(p: *mut holder_st) {
    DISPOSES.fetch_add(1, Ordering::SeqCst);
    // SAFETY: the caller hands a live holder.
    unsafe { (*p).parent = ptr::null() };
}

/// A lock that refuses a relock, as `CGuarded` requires: 0 when taken.
///
/// # Safety
///
/// `p` is a live holder.
unsafe fn holder_lock(p: *mut holder_st) -> c_int {
    LOCKS.fetch_add(1, Ordering::SeqCst);
    // SAFETY: the caller hands a live holder; one thread here.
    let locked = unsafe { &mut (*p).locked };
    if *locked {
        return -1;
    }
    *locked = true;
    0
}

/// # Safety
///
/// `p` is a live holder whose lock the caller holds.
unsafe fn holder_unlock(p: *mut holder_st) {
    // SAFETY: as above.
    unsafe { (*p).locked = false };
}

// ---------------------------------------------------------------------------
// Accessors, on the macro's handles as on any.
// ---------------------------------------------------------------------------

impl HolderRef<'_, '_> {
    fn parent(&self) -> u32 {
        // SAFETY: the handle addresses a live holder, whose parent outlives it
        // by `Holder`'s lifetime; reads only, through raw pointers.
        unsafe { *ptr::addr_of!((*self.as_ptr()).parent).read() }
    }
}

impl<'p> HolderMut<'_, 'p> {
    fn set_parent(&mut self, parent: &'p u32) {
        // SAFETY: a write through the exclusive handle's pointer; `'p` is the
        // lifetime the holder may keep it for.
        unsafe { ptr::addr_of_mut!((*self.as_mut_ptr()).parent).write(parent) }
    }
}

impl GuardedLocked<'_, '_> {
    fn is_locked(&mut self) -> bool {
        // SAFETY: the held lock covers the read.
        unsafe { (*self.as_mut_ptr()).locked }
    }
}

// The macro emits no `Debug`, so a wrapper writes the one it wants.
impl core::fmt::Debug for HolderRef<'_, '_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Holder")
            .field("parent", &self.parent())
            .finish()
    }
}

// SAFETY: `rc`, `locked` and `parent` are valid zeroed; no safe accessor
// dereferences a null parent in these tests.
unsafe impl CZeroable for Holder<'_> {}

// ---------------------------------------------------------------------------
// The policies, all from the macros.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default)]
pub struct HolderFree;
impl_cdrop!(HolderFree, Holder<'_>, holder_free);
impl_cdupclone!(<'p> HolderFree, Holder<'p>, holder_dup);

#[derive(Clone, Copy, Debug, Default)]
pub struct HolderUnref;
impl_cdrop!(HolderUnref, Holder<'_>, holder_free);
impl_crefclone!(
    HolderUnref,
    Holder<'_>,
    holder_up_ref,
    ok = |r| r == 0,
    sole = holder_is_sole
);

#[derive(Clone, Copy, Debug, Default)]
pub struct HolderDispose;
impl_cdispose!(HolderDispose, Holder<'_>, holder_dispose);

#[derive(Clone, Copy, Debug, Default)]
pub struct GuardedUnref;
impl_cdrop!(GuardedUnref, Guarded<'_>, holder_free);
impl_crefclone!(GuardedUnref, Guarded<'_>, holder_up_ref, ok = |r| r == 0);
impl_cguarded!(
    <'x> Guarded<'x>,
    GuardedLocked,
    lock = holder_lock,
    unlock = holder_unlock,
    ok = |r| r == 0,
);

impl_cguarded!(<'x> Whole<'x>, all, lock = holder_lock, unlock = holder_unlock, ok = |r| r == 0);

// ---------------------------------------------------------------------------
// The owners over a holder of a local: a lifetime shorter than `'static`.
// ---------------------------------------------------------------------------

#[test]
fn a_box_of_a_macro_type_frees_and_deep_copies() {
    let _g = serial();
    reset();
    let parent = 7_u32;
    {
        // SAFETY: a fresh holder, with the one reference this box takes.
        let b = unsafe { CBox::<Holder<'_>, HolderFree>::from_c(alloc(&parent)) }.unwrap();
        assert_eq!(b.as_ref().parent(), 7);
        let c = b.clone();
        assert_eq!(DUPS.load(Ordering::SeqCst), 1);
        assert_ne!(b.as_ptr(), c.as_ptr());
        assert_eq!(c.as_ref().parent(), 7);
        assert_eq!(format!("{:?}", c.as_ref()), "Holder { parent: 7 }");
    }
    assert_eq!(FREES.load(Ordering::SeqCst), 2);
}

#[test]
fn the_exclusive_handle_writes_and_reborrows() {
    let _g = serial();
    reset();
    let first = 1_u32;
    let second = 2_u32;
    {
        // SAFETY: a fresh holder, with the one reference this box takes.
        let mut b = unsafe { CBox::<Holder<'_>, HolderFree>::from_c(alloc(&first)) }.unwrap();
        let mut m = b.as_mut();
        m.as_mut().set_parent(&second); // a reborrow, so `m` stays usable
        assert_eq!(m.as_ref().parent(), 2);
        let raw = m.as_mut_ptr();
        // SAFETY: `raw` is the live holder `m` addresses, borrowed for the call.
        let again = unsafe { HolderMut::from_ptr(raw) }.unwrap();
        assert_eq!(again.as_ref().as_ptr(), raw.cast_const());
    }
    assert_eq!(FREES.load(Ordering::SeqCst), 1);
}

#[test]
fn an_arc_of_a_macro_type_counts() {
    let _g = serial();
    reset();
    let parent = 9_u32;
    {
        // SAFETY: a fresh holder, with the one reference this arc takes.
        let mut a = unsafe { CArc::<Holder<'_>, HolderUnref>::from_c(alloc(&parent)) }.unwrap();
        assert!(a.get_mut().is_some());
        let b = a.clone();
        assert_eq!(UP_REFS.load(Ordering::SeqCst), 1);
        assert!(a.get_mut().is_none());
        assert_eq!(b.as_ref().parent(), 9);
        drop(b);
        assert!(a.get_mut().is_some());
    }
    assert_eq!(FREES.load(Ordering::SeqCst), 2);
}

#[test]
fn a_value_of_a_macro_type_is_disposed_and_reaches_its_handles() {
    let _g = serial();
    reset();
    let parent = 3_u32;
    {
        let mut v = CVal::<Holder<'_>, HolderDispose>::zeroed();
        v.as_mut().set_parent(&parent);
        assert_eq!(v.as_ref().parent(), 3);
    }
    assert_eq!(DISPOSES.load(Ordering::SeqCst), 1);

    // A bare value reaches its handles through the macro's `as_ref` / `as_mut`.
    let mut h = <Holder<'_> as CZeroable>::zeroed();
    h.as_mut().set_parent(&parent);
    assert_eq!(h.as_ref().parent(), 3);
}

#[test]
fn a_lock_over_a_macro_type_hands_out_its_locked_handle() {
    let _g = serial();
    reset();
    let parent = 4_u32;
    {
        // SAFETY: a fresh holder, with the one reference this arc takes.
        let a = unsafe { CArc::<Guarded<'_>, GuardedUnref>::from_c(alloc(&parent)) }.unwrap();
        let mut g = a.lock();
        let mut l = g.as_locked();
        assert!(l.as_locked().is_locked());
        assert_eq!(l.as_ref().as_ptr(), a.as_ptr().cast_const().cast());
        drop(g);
        assert_eq!(LOCKS.load(Ordering::SeqCst), 1);
    }
    assert_eq!(FREES.load(Ordering::SeqCst), 1);
}

#[test]
fn a_whole_lock_over_a_macro_type_is_a_guarded_ref() {
    let _g = serial();
    reset();
    let parent = 6_u32;
    let raw = alloc(&parent);
    // SAFETY: `raw` is live until freed below, reached only through this
    // borrow, which every access locks.
    let r = unsafe { CGuardedRef::<Whole<'_>>::from_ptr(raw) }.unwrap();
    {
        let mut g = r.lock();
        let mut m = g.as_locked();
        // SAFETY: the held lock covers the read.
        assert!(unsafe { (*m.as_mut_ptr()).locked });
    }
    assert_eq!(LOCKS.load(Ordering::SeqCst), 1);
    // SAFETY: the guarded borrow is done with; the holder is ours to free.
    unsafe { holder_free(raw) };
}
