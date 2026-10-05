//! `CArc` and `CGuardedArc` over a mock refcounted C object with an atomic
//! count and its own reader/writer lock: cloning through the up_ref, the
//! sole-owner check behind `get_mut` / `make_mut`, and the guards — including
//! writers racing on several threads.

#![allow(non_camel_case_types, missing_docs)]

use core::ptr::{addr_of, addr_of_mut, NonNull};
use core::sync::atomic::{fence, AtomicIsize, AtomicUsize, Ordering};

use ffibox::{
    define_ctype, impl_cdrop, impl_cdupclone, impl_cguarded, impl_crefclone, CArc, CBox, CDrop,
    CGuardedArc, CGuardedRef, CRefClone, CWriteGuard,
};

// ---------------------------------------------------------------------------
// The mock C object
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct obj_st {
    rc: AtomicUsize,
    /// The lock: -1 while a writer holds it, otherwise the reader count.
    lock: AtomicIsize,
    read_locks: AtomicUsize,
    write_locks: AtomicUsize,
    value: u64,
    /// Counts frees; each test passes its own counter, so tests share nothing.
    freed: &'static AtomicUsize,
}

fn obj_new(value: u64, freed: &'static AtomicUsize) -> *mut obj_st {
    Box::into_raw(Box::new(obj_st {
        rc: AtomicUsize::new(1),
        lock: AtomicIsize::new(0),
        read_locks: AtomicUsize::new(0),
        write_locks: AtomicUsize::new(0),
        value,
        freed,
    }))
}

/// # Safety
///
/// `p` must be a live `obj_st` whose reference the caller holds.
unsafe fn obj_up_ref(p: *mut obj_st) {
    // SAFETY: the caller keeps `p` live.
    unsafe { (*p).rc.fetch_add(1, Ordering::Relaxed) };
}

/// # Safety
///
/// `p` must be a live `obj_st` whose reference the caller gives up.
unsafe fn obj_unref(p: *mut obj_st) {
    // SAFETY: the caller keeps `p` live until this down-ref.
    if unsafe { (*p).rc.fetch_sub(1, Ordering::Release) } == 1 {
        fence(Ordering::Acquire);
        // SAFETY: that was the last reference.
        let obj = unsafe { Box::from_raw(p) };
        obj.freed.fetch_add(1, Ordering::SeqCst);
    }
}

/// # Safety
///
/// `p` must be a live `obj_st` whose reference the caller holds.
unsafe fn obj_is_sole(p: *mut obj_st) -> bool {
    // SAFETY: the caller keeps `p` live.
    unsafe { (*p).rc.load(Ordering::Acquire) == 1 }
}

/// # Safety
///
/// `p` must be a live `obj_st` no one writes meanwhile.
unsafe fn obj_dup(p: *mut obj_st) -> *mut obj_st {
    // SAFETY: the caller keeps `p` live and quiescent.
    unsafe { obj_new(addr_of!((*p).value).read(), (*p).freed) }
}

/// # Safety
///
/// `p` must be a live `obj_st`.
unsafe fn obj_write_lock(p: *mut obj_st) {
    // References to the atomic fields only: a `&obj_st` would cover `value`,
    // which the current holder may be writing on another thread — the race
    // ffibox's no-reference rule exists to prevent.
    // SAFETY: the caller keeps `p` live.
    let (lock, write_locks) = unsafe { (&(*p).lock, &(*p).write_locks) };
    while lock
        .compare_exchange_weak(0, -1, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        std::thread::yield_now();
    }
    write_locks.fetch_add(1, Ordering::Relaxed);
}

/// # Safety
///
/// The caller must hold the write lock on a live `p`.
unsafe fn obj_write_unlock(p: *mut obj_st) {
    // SAFETY: the caller keeps `p` live.
    unsafe { (*p).lock.store(0, Ordering::Release) };
}

/// # Safety
///
/// `p` must be a live `obj_st`.
unsafe fn obj_read_lock(p: *mut obj_st) {
    // The atomic fields only, as in `obj_write_lock`.
    // SAFETY: the caller keeps `p` live.
    let (lock, read_locks) = unsafe { (&(*p).lock, &(*p).read_locks) };
    loop {
        let n = lock.load(Ordering::Relaxed);
        if n >= 0
            && lock
                .compare_exchange_weak(n, n + 1, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
        {
            break;
        }
        std::thread::yield_now();
    }
    read_locks.fetch_add(1, Ordering::Relaxed);
}

/// # Safety
///
/// The caller must hold a read lock on a live `p`.
unsafe fn obj_read_unlock(p: *mut obj_st) {
    // SAFETY: the caller keeps `p` live.
    unsafe { (*p).lock.fetch_sub(1, Ordering::Release) };
}

define_ctype!(Obj, ObjRef, ObjMut, obj_st);
// SAFETY: the count and the lock are atomic, and `value` is only written under
// the write lock or through a sole reference.
unsafe impl Send for Obj {}
// SAFETY: as above.
unsafe impl Sync for Obj {}
// SAFETY: `&T: Send` follows from `T: Sync`.
unsafe impl Send for ObjRef<'_> {}
// SAFETY: as above.
unsafe impl Sync for ObjRef<'_> {}
// SAFETY: `&mut T: Send` follows from `T: Send`.
unsafe impl Send for ObjMut<'_> {}
// SAFETY: as above.
unsafe impl Sync for ObjMut<'_> {}

impl_cguarded!(
    Obj,
    lock = obj_write_lock,
    unlock = obj_write_unlock,
    read_lock = obj_read_lock,
    read_unlock = obj_read_unlock,
);

impl ObjRef<'_> {
    fn value(&self) -> u64 {
        // SAFETY: a read through the handle's pointer; no reference is formed.
        unsafe { addr_of!((*self.as_ptr()).value).read() }
    }
    fn rc(&self) -> usize {
        // SAFETY: an atomic field, read through the handle's pointer.
        unsafe { (*self.as_ptr()).rc.load(Ordering::SeqCst) }
    }
    fn lock_state(&self) -> isize {
        // SAFETY: as above.
        unsafe { (*self.as_ptr()).lock.load(Ordering::SeqCst) }
    }
    fn locks(&self) -> (usize, usize) {
        // SAFETY: as above.
        unsafe {
            (
                (*self.as_ptr()).read_locks.load(Ordering::SeqCst),
                (*self.as_ptr()).write_locks.load(Ordering::SeqCst),
            )
        }
    }
}

impl ObjMut<'_> {
    fn set_value(&mut self, v: u64) {
        // SAFETY: a write through the exclusive handle's pointer.
        unsafe { addr_of_mut!((*self.as_mut_ptr()).value).write(v) }
    }
}

/// Down-ref, up_ref with a sole-owner check, and a deep copy.
#[derive(Clone, Copy, Debug, Default)]
pub struct ObjUnref;
impl_cdrop!(ObjUnref, Obj, obj_unref);
impl_crefclone!(ObjUnref, Obj, obj_up_ref, sole = obj_is_sole);
impl_cdupclone!(ObjUnref, Obj, obj_dup);

/// The same, but the count cannot be read: `c_is_sole_owner` stays `false`.
#[derive(Clone, Copy, Debug, Default)]
pub struct ObjUnrefBlind;
impl_cdrop!(ObjUnrefBlind, Obj, obj_unref);
impl_crefclone!(ObjUnrefBlind, Obj, obj_up_ref);
impl_cdupclone!(ObjUnrefBlind, Obj, obj_dup);

/// A drop-only share: a counted reference with no up_ref to call.
#[derive(Clone, Copy, Debug, Default)]
pub struct ObjDropOnly;
impl_cdrop!(ObjDropOnly, Obj, obj_unref);

type ObjArc = CArc<Obj, ObjUnref>;
type ObjGuarded = CGuardedArc<Obj, ObjUnref>;

fn arc(value: u64, freed: &'static AtomicUsize) -> ObjArc {
    // SAFETY: a fresh object whose one reference `obj_unref` releases.
    unsafe { ObjArc::from_c(obj_new(value, freed)) }.unwrap()
}

fn guarded(value: u64, freed: &'static AtomicUsize) -> ObjGuarded {
    // SAFETY: as `arc`.
    unsafe { ObjGuarded::from_c(obj_new(value, freed)) }.unwrap()
}

// ---------------------------------------------------------------------------
// CArc
// ---------------------------------------------------------------------------

#[test]
fn clones_share_the_object_and_the_last_drop_frees_it() {
    static FREED: AtomicUsize = AtomicUsize::new(0);
    let a = arc(1, &FREED);
    let b = a.clone();
    let c = b.try_clone().unwrap();
    assert_eq!(a.as_ptr(), c.as_ptr());
    assert_eq!(a.as_ref().rc(), 3);
    assert_eq!(c.as_ref().value(), 1);

    drop(a);
    drop(b);
    assert_eq!(FREED.load(Ordering::SeqCst), 0);
    drop(c);
    assert_eq!(FREED.load(Ordering::SeqCst), 1);
}

#[test]
fn get_mut_only_for_the_sole_reference() {
    static FREED: AtomicUsize = AtomicUsize::new(0);
    let mut a = arc(1, &FREED);
    a.get_mut().unwrap().set_value(2);
    assert_eq!(a.as_ref().value(), 2);

    let b = a.clone();
    assert!(a.get_mut().is_none(), "shared: no exclusive handle");
    drop(b);
    assert!(a.get_mut().is_some(), "sole again once the clone is gone");
}

#[test]
fn make_mut_copies_a_shared_object_and_leaves_the_others_alone() {
    static FREED: AtomicUsize = AtomicUsize::new(0);
    let a = arc(5, &FREED);
    let mut b = a.clone();
    b.make_mut().set_value(9);

    assert!(!ObjArc::ptr_eq(&a, &b), "the writer got its own copy");
    assert_eq!(a.as_ref().value(), 5);
    assert_eq!(b.as_ref().value(), 9);
    assert_eq!(a.as_ref().rc(), 1, "the copy released its old reference");
    assert_eq!(b.as_ref().rc(), 1);

    drop((a, b));
    assert_eq!(FREED.load(Ordering::SeqCst), 2);
}

#[test]
fn make_mut_on_the_sole_reference_copies_nothing() {
    static FREED: AtomicUsize = AtomicUsize::new(0);
    let mut a = arc(5, &FREED);
    let before = a.as_ptr();
    a.make_mut().set_value(6);
    assert_eq!(a.as_ptr(), before);
    assert_eq!(a.as_ref().value(), 6);
}

#[test]
fn without_a_sole_check_get_mut_declines_and_make_mut_always_copies() {
    static FREED: AtomicUsize = AtomicUsize::new(0);
    // SAFETY: a fresh object whose one reference `obj_unref` releases.
    let mut a = unsafe { CArc::<Obj, ObjUnrefBlind>::from_c(obj_new(1, &FREED)) }.unwrap();
    assert!(a.get_mut().is_none(), "the default check answers false");

    let before = a.as_ptr();
    a.make_mut().set_value(2);
    assert_ne!(a.as_ptr(), before, "copied, although it was sole");
    assert_eq!(FREED.load(Ordering::SeqCst), 1, "the original was released");
    assert_eq!(a.as_ref().value(), 2);
}

#[test]
fn a_box_shares_into_an_arc_and_a_sole_arc_unshares_into_a_box() {
    static FREED: AtomicUsize = AtomicUsize::new(0);
    // SAFETY: a fresh object, held as its only reference.
    let b = unsafe { CBox::<Obj, ObjUnref>::from_c(obj_new(3, &FREED)) }.unwrap();
    let a = ObjArc::from(b);
    let a2 = a.clone();

    assert!(ObjArc::ptr_eq(&a, &a2));
    let a = CBox::try_from(a).unwrap_err();
    drop(a2);
    let mut b: CBox<Obj, ObjUnref> = a.try_into().unwrap();
    b.as_mut().set_value(4);
    assert_eq!(b.as_ref().value(), 4);
    drop(b);
    assert_eq!(FREED.load(Ordering::SeqCst), 1);
}

#[test]
fn raw_seam_round_trips_one_reference() {
    static FREED: AtomicUsize = AtomicUsize::new(0);
    let a = arc(1, &FREED);
    let raw = a.into_c();
    // SAFETY: `raw` still owns the reference `into_c` gave up.
    assert_eq!(unsafe { (*raw).rc.load(Ordering::SeqCst) }, 1);
    // SAFETY: re-adopt it.
    let a = unsafe { ObjArc::from_c(raw) }.unwrap();
    assert_eq!(a.as_c_ptr(), raw);
    drop(a);
    assert_eq!(FREED.load(Ordering::SeqCst), 1);
}

#[test]
fn up_ref_is_settled_by_the_down_ref_through_the_trait() {
    static FREED: AtomicUsize = AtomicUsize::new(0);
    let a = arc(1, &FREED);
    let p = NonNull::new(a.as_ptr()).unwrap();
    // SAFETY: `a` keeps the object live; the bump is settled below.
    unsafe {
        assert!(a.policy().c_up_ref(p));
        assert!(!a.policy().c_is_sole_owner(p));
        a.policy().c_drop(p);
        assert!(a.policy().c_is_sole_owner(p));
    }
}

// ---------------------------------------------------------------------------
// CGuardedArc
// ---------------------------------------------------------------------------

#[test]
fn as_mut_holds_the_write_lock_until_the_guard_drops() {
    static FREED: AtomicUsize = AtomicUsize::new(0);
    let a = guarded(1, &FREED);
    let b = a.clone();
    {
        let mut w = b.write();
        w.as_mut().set_value(7);
        assert_eq!(w.as_ref().lock_state(), -1, "write-locked");
        assert_eq!(w.as_ref().value(), 7);
    }
    let r = a.read();
    assert_eq!(r.as_ref().lock_state(), 1, "unlocked, then read-locked");
    assert_eq!(r.as_ref().value(), 7, "the write is visible from the clone");
    assert_eq!(r.as_ref().locks(), (1, 1));
    drop(r);
    assert_eq!(a.read().as_ref().lock_state(), 1);
}

#[test]
fn readers_hold_the_lock_together() {
    static FREED: AtomicUsize = AtomicUsize::new(0);
    let a = guarded(1, &FREED);
    let b = a.clone();
    let r1 = a.read();
    let r2 = b.read();
    assert_eq!(r1.as_ref().lock_state(), 2);
    assert_eq!(r2.as_ref().value(), 1);
    drop((r1, r2));
    let w = a.write();
    assert_eq!(w.as_ref().lock_state(), -1);
}

#[test]
fn writers_on_many_threads_serialise_through_the_lock() {
    static FREED: AtomicUsize = AtomicUsize::new(0);
    const THREADS: u64 = 8;
    const ROUNDS: u64 = 2_000;
    let a = guarded(0, &FREED);

    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let mine = a.clone();
            std::thread::spawn(move || {
                for _ in 0..ROUNDS {
                    let mut w = mine.write();
                    // A read-modify-write that loses updates without the lock.
                    let v = w.as_ref().value();
                    std::hint::spin_loop();
                    w.as_mut().set_value(v + 1);
                }
                // Readers run alongside the other threads' writers.
                mine.read().as_ref().value()
            })
        })
        .collect();
    for h in handles {
        assert!(h.join().unwrap() <= THREADS * ROUNDS);
    }

    assert_eq!(a.read().as_ref().value(), THREADS * ROUNDS);
    assert_eq!(a.read().as_ref().rc(), 1);
    drop(a);
    assert_eq!(FREED.load(Ordering::SeqCst), 1);
}

#[test]
fn guarded_make_mut_copies_under_the_read_lock_and_unlocks() {
    static FREED: AtomicUsize = AtomicUsize::new(0);
    let a = guarded(5, &FREED);
    let mut b = a.clone();
    b.make_mut().set_value(6);

    let r = a.read();
    assert_eq!(r.as_ref().value(), 5);
    assert_eq!(r.as_ref().rc(), 1);
    // One read lock for the copy, one for `r`.
    assert_eq!(r.as_ref().locks(), (2, 0));
    assert_eq!(
        r.as_ref().lock_state(),
        1,
        "the copy's read lock was released"
    );
    drop(r);

    assert!(b.get_mut().is_some());
    assert_eq!(b.read().as_ref().value(), 6);
}

#[test]
fn guarded_make_mut_on_a_reference_it_cannot_prove_sole() {
    static FREED: AtomicUsize = AtomicUsize::new(0);
    // The blind policy copies even a sole reference, releasing the original —
    // its last reference — only after unlocking it.
    // SAFETY: a fresh object whose one reference `obj_unref` releases.
    let mut a = unsafe { CGuardedArc::<Obj, ObjUnrefBlind>::from_c(obj_new(1, &FREED)) }.unwrap();
    a.make_mut().set_value(2);
    assert_eq!(FREED.load(Ordering::SeqCst), 1);
    assert_eq!(a.read().as_ref().value(), 2);
}

// ---------------------------------------------------------------------------
// Thread-safety bounds and opt-ins
// ---------------------------------------------------------------------------

/// Compiles only when `T` is NOT `Send`: for a `Send` type both impls apply
/// and the `_` inference is ambiguous.
trait AmbiguousIfSend<A> {
    fn check() {}
}
impl<T: ?Sized> AmbiguousIfSend<()> for T {}
impl<T: ?Sized + Send> AmbiguousIfSend<u8> for T {}

/// As above, for `Clone`.
trait AmbiguousIfClone<A> {
    fn check() {}
}
impl<T> AmbiguousIfClone<()> for T {}
impl<T: Clone> AmbiguousIfClone<u8> for T {}

#[repr(C)]
pub struct mobile_st {
    _unused: [u8; 0],
}
define_ctype!(Mobile, MobileRef, MobileMut, mobile_st);
// SAFETY: test stand-in; thread-mobile but deliberately not `Sync`.
unsafe impl Send for Mobile {}

#[derive(Clone, Copy, Debug, Default)]
pub struct MobileUnref;
// SAFETY: never called; the types are only checked.
unsafe impl CDrop<Mobile> for MobileUnref {
    unsafe fn c_drop(&self, _: NonNull<Mobile>) {}
}
// SAFETY: as above.
unsafe impl CRefClone<Mobile> for MobileUnref {
    unsafe fn c_up_ref(&self, _: NonNull<Mobile>) -> bool {
        true
    }
}

fn is_send<T: Send>() {}
fn is_sync<T: Sync>() {}

#[test]
fn shared_owners_need_send_and_sync_where_a_box_needs_send() {
    is_send::<CBox<Mobile, MobileUnref>>();
    <CArc<Mobile, MobileUnref> as AmbiguousIfSend<_>>::check();
    is_send::<ObjArc>();
    is_sync::<ObjArc>();
    is_send::<ObjGuarded>();
    is_sync::<ObjGuarded>();
}

#[test]
fn guards_stay_on_the_locking_thread() {
    <CWriteGuard<'_, Obj> as AmbiguousIfSend<_>>::check();
    <ffibox::CReadGuard<'_, Obj> as AmbiguousIfSend<_>>::check();
    is_sync::<CWriteGuard<'_, Obj>>();
}

#[test]
fn a_drop_only_share_is_held_but_not_cloned() {
    static FREED: AtomicUsize = AtomicUsize::new(0);
    <CArc<Obj, ObjDropOnly> as AmbiguousIfClone<_>>::check();
    // SAFETY: a fresh object whose one reference `obj_unref` releases.
    let a = unsafe { CArc::<Obj, ObjDropOnly>::from_c(obj_new(1, &FREED)) }.unwrap();
    assert_eq!(a.as_ref().value(), 1);
    drop(a);
    assert_eq!(FREED.load(Ordering::SeqCst), 1);
}

#[test]
fn a_zst_policy_keeps_the_arcs_pointer_sized() {
    use core::mem::size_of;
    assert_eq!(size_of::<ObjArc>(), size_of::<*mut obj_st>());
    assert_eq!(size_of::<Option<ObjGuarded>>(), size_of::<*mut obj_st>());
}

/// A lock whose C call always fails.
#[repr(C)]
pub struct broken_st {
    _unused: [u8; 0],
}
define_ctype!(Broken, BrokenRef, BrokenMut, broken_st);
// SAFETY: `c_lock` never succeeds, so no guard is ever handed out.
unsafe impl ffibox::CGuarded for Broken {
    unsafe fn c_lock(_: NonNull<Self>) -> bool {
        false
    }
    unsafe fn c_unlock(_: NonNull<Self>) {
        unreachable!("never locked")
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct BrokenFree;
// SAFETY: frees the Box `a_failing_lock_call_panics` allocated.
unsafe impl CDrop<Broken> for BrokenFree {
    unsafe fn c_drop(&self, ptr: NonNull<Broken>) {
        // SAFETY: the caller transfers the allocation.
        drop(unsafe { Box::from_raw(ptr.as_ptr().cast::<[u8; 1]>()) });
    }
}

#[test]
fn a_failing_lock_call_panics_and_leaves_nothing_locked() {
    let raw = Box::into_raw(Box::new([0u8; 1])).cast::<Broken>();
    // SAFETY: a fresh allocation `BrokenFree` releases.
    let a = unsafe { CGuardedArc::<Broken, BrokenFree>::from_raw(raw) }.unwrap();
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(a.write())));
    assert!(r.is_err(), "write must panic when the lock call fails");
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(a.read())));
    assert!(r.is_err(), "read falls back to the failing exclusive lock");
    drop(a); // the arc itself is still released normally
}

// ---------------------------------------------------------------------------
// `ok = …`: bound routines that report a status
// ---------------------------------------------------------------------------

/// A lock that, like glibc's `pthread_rwlock_*`, reports `EDEADLK` instead of
/// blocking when it is already held, and an up_ref that can fail.
#[repr(C)]
pub struct status_st {
    /// -1 while a writer holds it, otherwise the reader count.
    lock: AtomicIsize,
    rc: AtomicUsize,
    /// Whether the next up_ref fails.
    fail_up_ref: bool,
}

const EDEADLK: i32 = 35;

/// # Safety
///
/// `p` must point to a live `status_st`.
unsafe fn status_write_lock(p: *mut status_st) -> i32 {
    // SAFETY: the caller keeps `p` live.
    let lock = unsafe { &(*p).lock };
    match lock.compare_exchange(0, -1, Ordering::Acquire, Ordering::Relaxed) {
        Ok(_) => 0,
        Err(_) => EDEADLK, // single-threaded test: held means held by us
    }
}

/// # Safety
///
/// `p` must point to a live `status_st`.
unsafe fn status_read_lock(p: *mut status_st) -> i32 {
    // SAFETY: the caller keeps `p` live.
    let lock = unsafe { &(*p).lock };
    if lock.load(Ordering::Relaxed) < 0 {
        return EDEADLK;
    }
    lock.fetch_add(1, Ordering::Acquire);
    0
}

/// # Safety
///
/// The caller must hold a lock on a live `p`.
unsafe fn status_unlock(p: *mut status_st) -> i32 {
    // SAFETY: the caller keeps `p` live.
    let lock = unsafe { &(*p).lock };
    if lock.load(Ordering::Relaxed) < 0 {
        lock.store(0, Ordering::Release);
    } else {
        lock.fetch_sub(1, Ordering::Release);
    }
    0
}

/// # Safety
///
/// `p` must point to a live `status_st`.
unsafe fn status_up_ref(p: *mut status_st) -> i32 {
    // SAFETY: the caller keeps `p` live; fields only, as in `obj_write_lock`.
    let (fail, rc) = unsafe { (addr_of!((*p).fail_up_ref).read(), &(*p).rc) };
    if fail {
        return 0;
    }
    rc.fetch_add(1, Ordering::Relaxed);
    1
}

/// # Safety
///
/// `p` must point to a live `status_st` owning one reference.
unsafe fn status_unref(p: *mut status_st) {
    // SAFETY: the caller keeps `p` live and transfers one reference.
    if unsafe { (*p).rc.fetch_sub(1, Ordering::Release) } == 1 {
        fence(Ordering::Acquire);
        // SAFETY: the last reference; allocated by `status_new`.
        drop(unsafe { Box::from_raw(p) });
    }
}

fn status_new(fail_up_ref: bool) -> *mut status_st {
    Box::into_raw(Box::new(status_st {
        lock: AtomicIsize::new(0),
        rc: AtomicUsize::new(1),
        fail_up_ref,
    }))
}

define_ctype!(Status, StatusRef, StatusMut, status_st);
impl_cguarded!(
    Status,
    lock = status_write_lock,
    unlock = status_unlock,
    read_lock = status_read_lock,
    read_unlock = status_unlock,
    ok = |r| r == 0,
);

#[derive(Clone, Copy, Debug, Default)]
pub struct StatusUnref;
impl_cdrop!(StatusUnref, Status, status_unref);
impl_crefclone!(StatusUnref, Status, status_up_ref, ok = |r| r == 1);

#[test]
fn a_relock_reported_as_edeadlk_panics_instead_of_aliasing() {
    // SAFETY: a fresh object with one reference `status_unref` releases.
    let a = unsafe { CGuardedArc::<Status, StatusUnref>::from_c(status_new(false)) }.unwrap();
    let mut first = a.write();
    let _held = first.as_mut();
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(a.write())));
    assert!(r.is_err(), "a second write on this thread must not succeed");
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(a.read())));
    assert!(
        r.is_err(),
        "a read under this thread's write lock must not succeed"
    );
    drop(first);
    drop(a.read()); // the lock was left consistent
}

#[test]
fn a_failed_up_ref_reported_through_ok_is_a_failed_clone() {
    // SAFETY: a fresh object with one reference `status_unref` releases.
    let a = unsafe { CArc::<Status, StatusUnref>::from_c(status_new(true)) }.unwrap();
    assert!(a.try_clone().is_none());
    // SAFETY: a fresh object with one reference `status_unref` releases.
    let b = unsafe { CArc::<Status, StatusUnref>::from_c(status_new(false)) }.unwrap();
    let c = b.try_clone().expect("up_ref succeeded");
    assert!(CArc::ptr_eq(&b, &c));
}

// ---------------------------------------------------------------------------
// `CGuardedRef`: a C global reached through its own lock
// ---------------------------------------------------------------------------

/// A C global as the FFI sees one: a `static mut` reached only by address.
macro_rules! global_obj {
    ($name:ident) => {
        static mut $name: obj_st = obj_st {
            rc: AtomicUsize::new(1),
            lock: AtomicIsize::new(0),
            read_locks: AtomicUsize::new(0),
            write_locks: AtomicUsize::new(0),
            value: 0,
            freed: {
                static NEVER: AtomicUsize = AtomicUsize::new(0);
                &NEVER
            },
        };
    };
}

/// The `'static` view a wrapper would expose for the global.
fn global_view(p: *mut obj_st) -> CGuardedRef<'static, Obj> {
    // SAFETY: a static lives for the program, and in these tests every access
    // goes through the view, which locks.
    unsafe { CGuardedRef::from_ptr(p) }.expect("a static's address is non-null")
}

#[test]
fn a_global_is_read_and_written_under_its_lock() {
    global_obj!(G);
    let g = global_view(core::ptr::addr_of_mut!(G));
    g.write().as_mut().set_value(5);
    let r = g.read(); // `read` takes the view by value: the guard keeps `'static`
    assert_eq!(r.as_ref().value(), 5);
    assert_eq!(r.as_ref().lock_state(), 1, "one reader holds the lock");
    drop(r);
    let state = g.read().as_ref().locks();
    assert_eq!(state, (2, 1), "every access took the lock");
    assert_eq!(g.read().as_ref().lock_state(), 1);
    assert_eq!(g.as_c_ptr(), core::ptr::addr_of_mut!(G));
}

#[test]
fn copies_of_a_global_view_serialise_writers_across_threads() {
    global_obj!(G);
    const THREADS: u64 = 4;
    const ROUNDS: u64 = 200;
    let g = global_view(core::ptr::addr_of_mut!(G));
    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            // `Copy` and `Send`: each thread gets its own copy of the view.
            std::thread::spawn(move || {
                for _ in 0..ROUNDS {
                    let mut w = g.write();
                    // A read-modify-write that loses updates without the lock.
                    let v = w.as_ref().value();
                    std::hint::spin_loop();
                    w.as_mut().set_value(v + 1);
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(g.read().as_ref().value(), THREADS * ROUNDS);
}

#[test]
fn a_null_global_is_rejected() {
    // SAFETY: null is rejected before anything is borrowed.
    assert!(unsafe { CGuardedRef::<Obj>::from_ptr(core::ptr::null_mut()) }.is_none());
}

#[test]
fn a_global_relock_reported_through_ok_panics() {
    static mut S: status_st = status_st {
        lock: AtomicIsize::new(0),
        rc: AtomicUsize::new(1),
        fail_up_ref: false,
    };
    // SAFETY: a static, reached only through this view.
    let g = unsafe { CGuardedRef::<Status>::from_ptr(core::ptr::addr_of_mut!(S)) }.unwrap();
    let held = g.write();
    let r = std::panic::catch_unwind(|| drop(g.write()));
    assert!(r.is_err(), "a second write on this thread must not succeed");
    drop(held);
    drop(g.write()); // the lock was left consistent
}
