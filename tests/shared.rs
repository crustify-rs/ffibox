//! `CArc` over a mock refcounted C object with an atomic count and a lock over
//! one of its fields: cloning through the up_ref, the sole-owner check behind
//! `get_mut` / `make_mut`, and `lock` — including writers racing on several
//! threads while readers of the unlocked state run alongside. Then
//! `CGuardedRef` over a global whose lock covers all of it.

#![allow(non_camel_case_types, missing_docs)]

use core::ptr::{addr_of, addr_of_mut, NonNull};
use core::sync::atomic::{fence, AtomicBool, AtomicUsize, Ordering};

use ffibox::{
    define_ctype, impl_cdrop, impl_cdupclone, impl_cguarded, impl_crefclone, CArc, CBox, CDrop,
    CGuard, CGuardedRef, CRefClone,
};

// ---------------------------------------------------------------------------
// The mock C object
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct obj_st {
    rc: AtomicUsize,
    /// The mutex, and how many times it was taken.
    held: AtomicBool,
    locks: AtomicUsize,
    /// Fixed once the object is shared: read without the lock.
    value: u64,
    /// Written by every holder after sharing: only under the lock.
    counter: u64,
    /// Counts frees; each test passes its own counter, so tests share nothing.
    freed: &'static AtomicUsize,
}

fn obj_new(value: u64, freed: &'static AtomicUsize) -> *mut obj_st {
    Box::into_raw(Box::new(obj_st {
        rc: AtomicUsize::new(1),
        held: AtomicBool::new(false),
        locks: AtomicUsize::new(0),
        value,
        counter: 0,
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
/// `p` must be a live `obj_st`.
unsafe fn obj_lock(p: *mut obj_st) {
    // References to the atomic fields only: a `&obj_st` would cover `counter`,
    // which the current holder may be writing on another thread — the race
    // ffibox's no-reference rule exists to prevent.
    // SAFETY: the caller keeps `p` live.
    let (held, locks) = unsafe { (&(*p).held, &(*p).locks) };
    while held
        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        std::thread::yield_now();
    }
    locks.fetch_add(1, Ordering::Relaxed);
}

/// # Safety
///
/// The caller must hold the lock on a live `p`.
unsafe fn obj_unlock(p: *mut obj_st) {
    // SAFETY: the caller keeps `p` live.
    unsafe { (*p).held.store(false, Ordering::Release) };
}

/// A copy routine that, like a C `*_dup` on a locked object, takes the lock
/// around the state it protects — what `CGuarded`'s contract asks of a
/// `CDupClone` policy.
///
/// # Safety
///
/// `p` must be a live `obj_st`.
unsafe fn obj_dup(p: *mut obj_st) -> *mut obj_st {
    // SAFETY: the caller keeps `p` live; `value` is fixed once shared.
    let copy = unsafe { obj_new(addr_of!((*p).value).read(), (*p).freed) };
    // SAFETY: as above; `counter` is read under the lock.
    unsafe {
        obj_lock(p);
        let counter = addr_of!((*p).counter).read();
        obj_unlock(p);
        addr_of_mut!((*copy).counter).write(counter);
    }
    copy
}

define_ctype!(Obj, ObjRef, ObjMut, obj_st);
// SAFETY: the count and the lock are atomic, `value` is written only through
// a sole reference, and `counter` only under the lock or through one.
unsafe impl Send for Obj {}
// SAFETY: as above.
unsafe impl Sync for Obj {}
// SAFETY: `&T: Send` follows from `T: Sync`.
unsafe impl Send for ObjRef<'_> {}
// SAFETY: as above.
unsafe impl Sync for ObjRef<'_> {}

impl_cguarded!(Obj, ObjLocked, lock = obj_lock, unlock = obj_unlock);

impl ObjRef<'_> {
    /// Unlocked state: fixed once the object is shared.
    fn value(&self) -> u64 {
        // SAFETY: a read through the handle's pointer; no reference is formed.
        unsafe { addr_of!((*self.as_ptr()).value).read() }
    }
    fn rc(&self) -> usize {
        // SAFETY: an atomic field, read through the handle's pointer.
        unsafe { (*self.as_ptr()).rc.load(Ordering::SeqCst) }
    }
    fn held(&self) -> bool {
        // SAFETY: as above.
        unsafe { (*self.as_ptr()).held.load(Ordering::SeqCst) }
    }
    fn locks(&self) -> usize {
        // SAFETY: as above.
        unsafe { (*self.as_ptr()).locks.load(Ordering::SeqCst) }
    }
}

impl ObjMut<'_> {
    fn set_value(&mut self, v: u64) {
        // SAFETY: a write through the exclusive handle's pointer.
        unsafe { addr_of_mut!((*self.as_mut_ptr()).value).write(v) }
    }
}

impl ObjLocked<'_> {
    /// Locked state: read as well as written under the lock.
    fn counter(&mut self) -> u64 {
        // SAFETY: the lock is held for the handle's life.
        unsafe { addr_of!((*self.as_mut_ptr()).counter).read() }
    }
    fn set_counter(&mut self, v: u64) {
        // SAFETY: as above.
        unsafe { addr_of_mut!((*self.as_mut_ptr()).counter).write(v) }
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

fn arc(value: u64, freed: &'static AtomicUsize) -> ObjArc {
    // SAFETY: a fresh object whose one reference `obj_unref` releases.
    unsafe { ObjArc::from_c(obj_new(value, freed)) }.unwrap()
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
// CArc::lock
// ---------------------------------------------------------------------------

#[test]
fn lock_holds_the_lock_until_the_guard_drops() {
    static FREED: AtomicUsize = AtomicUsize::new(0);
    let a = arc(1, &FREED);
    let b = a.clone();
    {
        let mut g = b.lock();
        g.as_locked().set_counter(7);
        assert!(g.as_ref().held(), "locked");
        assert!(a.as_ref().held(), "for every clone");
        assert_eq!(g.as_locked().counter(), 7);
        assert_eq!(a.as_ref().value(), 1, "unlocked state stays readable");
    }
    assert!(!a.as_ref().held(), "the guard unlocked on drop");
    assert_eq!(a.lock().as_locked().counter(), 7, "visible from the clone");
    assert_eq!(a.as_ref().locks(), 2);
}

#[test]
fn a_locked_handle_reborrows_and_reaches_the_getters() {
    static FREED: AtomicUsize = AtomicUsize::new(0);
    let a = arc(3, &FREED);
    let mut g = a.lock();
    let mut h = g.as_locked();
    fn bump(mut h: ObjLocked<'_>) {
        let v = h.counter();
        h.set_counter(v + 1);
    }
    bump(h.as_locked());
    bump(h.as_locked());
    assert_eq!(h.counter(), 2);
    assert_eq!(h.as_ref().value(), 3);
    assert_eq!(g.as_ptr(), a.as_ptr());
}

#[test]
fn writers_on_many_threads_serialise_through_the_lock() {
    static FREED: AtomicUsize = AtomicUsize::new(0);
    const THREADS: u64 = 8;
    const ROUNDS: u64 = 2_000;
    let a = arc(42, &FREED);

    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let mine = a.clone();
            std::thread::spawn(move || {
                let mut seen = 0;
                for _ in 0..ROUNDS {
                    {
                        let mut g = mine.lock();
                        let mut h = g.as_locked();
                        // A read-modify-write that loses updates without the lock.
                        let v = h.counter();
                        std::hint::spin_loop();
                        h.set_counter(v + 1);
                    }
                    // The unlocked state is read alongside the other writers.
                    seen += mine.as_ref().value();
                }
                seen
            })
        })
        .collect();
    for h in handles {
        assert_eq!(h.join().unwrap(), 42 * ROUNDS);
    }

    assert_eq!(a.lock().as_locked().counter(), THREADS * ROUNDS);
    assert_eq!(a.as_ref().rc(), 1);
    drop(a);
    assert_eq!(FREED.load(Ordering::SeqCst), 1);
}

#[test]
fn make_mut_copies_the_locked_state_under_the_lock() {
    static FREED: AtomicUsize = AtomicUsize::new(0);
    let a = arc(5, &FREED);
    a.lock().as_locked().set_counter(4);
    let mut b = a.clone();
    b.make_mut().set_value(6);

    assert_eq!(a.as_ref().locks(), 2, "the copy took the original's lock");
    assert!(!a.as_ref().held(), "and released it");
    assert_eq!(a.as_ref().value(), 5);
    assert_eq!(b.as_ref().value(), 6);
    assert_eq!(b.lock().as_locked().counter(), 4);
    assert!(b.get_mut().is_some());
}

#[test]
fn get_mut_reaches_the_locked_state_without_the_lock() {
    static FREED: AtomicUsize = AtomicUsize::new(0);
    let mut a = arc(1, &FREED);
    // Sole: no other reference can take the lock, so none is needed.
    a.get_mut().unwrap().set_value(2);
    assert_eq!(a.as_ref().locks(), 0);
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
}

#[test]
fn guards_stay_on_the_locking_thread() {
    <CGuard<'_, Obj> as AmbiguousIfSend<_>>::check();
    is_sync::<CGuard<'_, Obj>>();
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
    assert_eq!(size_of::<Option<ObjArc>>(), size_of::<*mut obj_st>());
    assert_eq!(size_of::<ObjLocked<'_>>(), size_of::<*mut obj_st>());
}

/// A lock whose C call always fails.
#[repr(C)]
pub struct broken_st {
    _unused: [u8; 0],
}
define_ctype!(Broken, BrokenRef, BrokenMut, broken_st);
// SAFETY: `c_lock` never succeeds, so no guard is ever handed out.
unsafe impl ffibox::CGuarded for Broken {
    type Locked<'a> = BrokenMut<'a>;
    type Scope = ffibox::LockFields;
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
    let a = unsafe { CArc::<Broken, BrokenFree>::from_raw(raw) }.unwrap();
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(a.lock())));
    assert!(r.is_err(), "lock must panic when the lock call fails");
    drop(a); // the arc itself is still released normally
}

// ---------------------------------------------------------------------------
// `ok = …`: bound routines that report a status
// ---------------------------------------------------------------------------

/// A lock that, like an error-checking `pthread_mutex_*`, reports `EDEADLK`
/// instead of blocking when it is already held, and an up_ref that can fail.
#[repr(C)]
pub struct status_st {
    held: AtomicBool,
    rc: AtomicUsize,
    /// Whether the next up_ref fails.
    fail_up_ref: bool,
}

const EDEADLK: i32 = 35;

/// # Safety
///
/// `p` must point to a live `status_st`.
unsafe fn status_lock(p: *mut status_st) -> i32 {
    // SAFETY: the caller keeps `p` live.
    let held = unsafe { &(*p).held };
    match held.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed) {
        Ok(_) => 0,
        Err(_) => EDEADLK, // single-threaded test: held means held by us
    }
}

/// # Safety
///
/// The caller must hold the lock on a live `p`.
unsafe fn status_unlock(p: *mut status_st) -> i32 {
    // SAFETY: the caller keeps `p` live.
    unsafe { (*p).held.store(false, Ordering::Release) };
    0
}

/// # Safety
///
/// `p` must point to a live `status_st`.
unsafe fn status_up_ref(p: *mut status_st) -> i32 {
    // SAFETY: the caller keeps `p` live; fields only, as in `obj_lock`.
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
        held: AtomicBool::new(false),
        rc: AtomicUsize::new(1),
        fail_up_ref,
    }))
}

define_ctype!(Status, StatusRef, StatusMut, status_st);
impl_cguarded!(
    Status,
    StatusLocked,
    lock = status_lock,
    unlock = status_unlock,
    ok = |r| r == 0,
);

#[derive(Clone, Copy, Debug, Default)]
pub struct StatusUnref;
impl_cdrop!(StatusUnref, Status, status_unref);
impl_crefclone!(StatusUnref, Status, status_up_ref, ok = |r| r == 1);

#[test]
fn a_relock_reported_as_edeadlk_panics_instead_of_aliasing() {
    // SAFETY: a fresh object with one reference `status_unref` releases.
    let a = unsafe { CArc::<Status, StatusUnref>::from_c(status_new(false)) }.unwrap();
    let b = a.clone();
    let mut first = a.lock();
    let _held = first.as_locked();
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(b.lock())));
    assert!(r.is_err(), "a second lock on this thread must not succeed");
    drop(first);
    drop(b.lock()); // the lock was left consistent
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
// `CGuardedRef`: a C global reached only through the lock covering all of it
// ---------------------------------------------------------------------------

// The same C struct, bound as a whole-object lock: no unlocked handle.
define_ctype!(Reg, RegRef, RegMut, obj_st);
// SAFETY: every field is reached under the lock, one thread at a time.
unsafe impl Send for Reg {}
impl_cguarded!(Reg, all, lock = obj_lock, unlock = obj_unlock);

impl RegRef<'_> {
    fn value(&self) -> u64 {
        // SAFETY: a read through the handle's pointer, under the lock.
        unsafe { addr_of!((*self.as_ptr()).value).read() }
    }
    fn locks(&self) -> usize {
        // SAFETY: an atomic field, read through the handle's pointer.
        unsafe { (*self.as_ptr()).locks.load(Ordering::SeqCst) }
    }
}

impl RegMut<'_> {
    fn set_value(&mut self, v: u64) {
        // SAFETY: a write through the exclusive handle's pointer, under the lock.
        unsafe { addr_of_mut!((*self.as_mut_ptr()).value).write(v) }
    }
}

/// A C global as the FFI sees one: a `static mut` reached only by address.
macro_rules! global_obj {
    ($name:ident) => {
        static mut $name: obj_st = obj_st {
            rc: AtomicUsize::new(1),
            held: AtomicBool::new(false),
            locks: AtomicUsize::new(0),
            value: 0,
            counter: 0,
            freed: {
                static NEVER: AtomicUsize = AtomicUsize::new(0);
                &NEVER
            },
        };
    };
}

/// The `'static` view a wrapper would expose for the global.
fn global_view(p: *mut obj_st) -> CGuardedRef<'static, Reg> {
    // SAFETY: a static lives for the program, and in these tests every access
    // goes through the view, which locks.
    unsafe { CGuardedRef::from_ptr(p) }.expect("a static's address is non-null")
}

#[test]
fn a_global_is_read_and_written_under_its_lock() {
    global_obj!(G);
    let g = global_view(core::ptr::addr_of_mut!(G));
    g.lock().as_locked().set_value(5);
    let r = g.lock(); // `lock` takes the view by value: the guard keeps `'static`
    assert_eq!(r.as_ref().value(), 5);
    assert_eq!(r.as_ref().locks(), 2, "every access took the lock");
    drop(r);
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
                    let mut l = g.lock();
                    let mut w = l.as_locked();
                    // A read-modify-write that loses updates without the lock.
                    let v = w.as_ref().value();
                    std::hint::spin_loop();
                    w.set_value(v + 1);
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(g.lock().as_ref().value(), THREADS * ROUNDS);
}

#[test]
fn a_null_global_is_rejected() {
    // SAFETY: null is rejected before anything is borrowed.
    assert!(unsafe { CGuardedRef::<Reg>::from_ptr(core::ptr::null_mut()) }.is_none());
}

define_ctype!(StatusAll, StatusAllRef, StatusAllMut, status_st);
impl_cguarded!(
    StatusAll,
    all,
    lock = status_lock,
    unlock = status_unlock,
    ok = |r| r == 0,
);

#[test]
fn a_global_relock_reported_through_ok_panics() {
    static mut S: status_st = status_st {
        held: AtomicBool::new(false),
        rc: AtomicUsize::new(1),
        fail_up_ref: false,
    };
    // SAFETY: a static, reached only through this view.
    let g = unsafe { CGuardedRef::<StatusAll>::from_ptr(core::ptr::addr_of_mut!(S)) }.unwrap();
    let held = g.lock();
    let r = std::panic::catch_unwind(|| drop(g.lock()));
    assert!(r.is_err(), "a second lock on this thread must not succeed");
    drop(held);
    drop(g.lock()); // the lock was left consistent
}
