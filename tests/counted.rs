//! `CShared` over Rust objects whose count is their own field, which C takes
//! and drops too: libgit2's `git_refcount { refcount; owner; }`, with an
//! owner that keeps the object alive past its last counted reference, a
//! release that frees through another allocation family, a type C writes
//! beyond its count (no `Deref`), and references taken and dropped from
//! several threads and through the C-side routines.

#![allow(missing_docs)]

use core::alloc::{AllocError, Allocator, Layout};
use core::ffi::c_void;
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicI32, AtomicPtr, AtomicUsize, Ordering};

use ffibox::{CAtomicOwner, CCounted, CShareable, CShared};

// ---------------------------------------------------------------------------
// `git_refcount`, and an object that starts with one
// ---------------------------------------------------------------------------

/// `git_refcount`: the count and the owner, both atomic.
#[repr(C)]
pub struct Refcount {
    count: AtomicI32,
    owner: AtomicPtr<c_void>,
}

impl Refcount {
    const fn one() -> Self {
        Refcount {
            count: AtomicI32::new(1),
            owner: AtomicPtr::new(ptr::null_mut()),
        }
    }
    /// `GIT_REFCOUNT_INC`.
    fn inc(&self) {
        self.count.fetch_add(1, Ordering::Relaxed);
    }
    /// `GIT_REFCOUNT_DEC` without the free: whether to free now.
    fn dec(&self) -> bool {
        let v = self.count.fetch_sub(1, Ordering::AcqRel) - 1;
        v <= 0 && self.owner.load(Ordering::Acquire).is_null()
    }
}

/// A counted object, `git_odb`-like: the C-visible count, then Rust state
/// that holders share through atomics.
#[repr(C)]
pub struct Odb {
    rc: Refcount,
    reads: AtomicUsize,
    freed: &'static AtomicUsize,
}

impl Drop for Odb {
    fn drop(&mut self) {
        self.freed.fetch_add(1, Ordering::Relaxed);
    }
}

/// The count of the live object at `this`.
///
/// # Safety
///
/// `this` is live for `'a`.
unsafe fn rc<'a, T>(this: NonNull<T>) -> &'a Refcount {
    // SAFETY: every counted type here starts with its `Refcount`, which is
    // all atomics.
    unsafe { this.cast::<Refcount>().as_ref() }
}

// SAFETY: `rc` counts the references as `GIT_REFCOUNT_*` do, atomically; an
// object with an owner is released by the owner; `Odb::new` is the only
// constructor, with a count of 1 and no owner; objects live in a `Box`.
unsafe impl CCounted for Odb {
    type Alloc = std::alloc::Global;
    unsafe fn c_up_ref(this: NonNull<Self>) {
        // SAFETY: a live object (the caller's reference).
        unsafe { rc(this) }.inc();
    }
    unsafe fn c_down_ref(this: NonNull<Self>) -> bool {
        // SAFETY: as above.
        unsafe { rc(this) }.dec()
    }
    unsafe fn c_is_sole(this: NonNull<Self>) -> bool {
        // SAFETY: as above.
        let rc = unsafe { rc(this) };
        rc.count.load(Ordering::Acquire) == 1 && rc.owner.load(Ordering::Acquire).is_null()
    }
}
// SAFETY: C writes `rc` only, and holders `reads`, both atomic.
unsafe impl CShareable for Odb {}

impl Odb {
    fn new(freed: &'static AtomicUsize) -> CShared<Odb> {
        CShared::from_box(Box::new(Odb {
            rc: Refcount::one(),
            reads: AtomicUsize::new(0),
            freed,
        }))
    }
    fn read(&self) -> usize {
        self.reads.fetch_add(1, Ordering::Relaxed) + 1
    }
}

/// The C side: `git_odb_free`, and C's own `GIT_REFCOUNT_INC`.
mod c {
    use super::*;

    pub unsafe extern "C" fn git_odb_free(db: *mut Odb) {
        // SAFETY: C passes NULL or a reference it gives up.
        unsafe { CShared::release(db) }
    }

    /// What C's inline macro does to the field directly.
    pub unsafe fn refcount_inc(db: *mut Odb) {
        // SAFETY: C's caller holds a reference.
        unsafe { (*db).rc.count.fetch_add(1, Ordering::Relaxed) };
    }
}

/// A free counter of its own for each test that names one: a static, so
/// that nothing leaks.
macro_rules! counter {
    () => {{
        static FREED: AtomicUsize = AtomicUsize::new(0);
        &FREED
    }};
}

#[test]
fn clone_and_drop_count_references() {
    let freed = counter!();
    let a = Odb::new(freed);
    let b = a.clone();
    assert!(CShared::ptr_eq(&a, &b));
    assert_eq!(a.read(), 1);
    assert_eq!(b.read(), 2);
    drop(a);
    assert_eq!(freed.load(Ordering::Relaxed), 0);
    drop(b);
    assert_eq!(freed.load(Ordering::Relaxed), 1);
}

#[test]
fn references_cross_to_c_and_back() {
    let freed = counter!();
    let a = Odb::new(freed);
    // Handed to C, which keeps it and takes one more with its own macro.
    let p = a.clone().into_raw();
    // SAFETY: `p` is live, C holds a reference.
    unsafe { c::refcount_inc(p) };
    // C gives one back through Rust, then frees the other.
    // SAFETY: C's reference, adopted once.
    let back = unsafe { CShared::from_raw(p) }.unwrap();
    assert!(CShared::ptr_eq(&a, &back));
    // SAFETY: C's other reference, given up.
    unsafe { c::git_odb_free(p) };
    // SAFETY: NULL is ignored.
    unsafe { c::git_odb_free(ptr::null_mut()) };
    drop(back);
    assert_eq!(freed.load(Ordering::Relaxed), 0);
    // A pointer C only lends becomes a reference of Rust's own.
    // SAFETY: `a` keeps the object alive.
    let lent = unsafe { CShared::from_borrowed(a.as_ptr()) }.unwrap();
    drop(a);
    assert_eq!(lent.read(), 1);
    drop(lent);
    assert_eq!(freed.load(Ordering::Relaxed), 1);
}

#[test]
fn increment_and_decrement_count_as_arc() {
    let freed = counter!();
    let a = Odb::new(freed);
    let p = a.into_raw();
    // SAFETY: `p` holds a reference.
    unsafe { CShared::increment_count(p) };
    // SAFETY: one of the two references.
    unsafe { CShared::decrement_count(p) };
    assert_eq!(freed.load(Ordering::Relaxed), 0);
    // SAFETY: the other.
    unsafe { CShared::decrement_count(p) };
    assert_eq!(freed.load(Ordering::Relaxed), 1);
}

#[test]
fn get_mut_only_when_sole() {
    let freed = counter!();
    let mut a = Odb::new(freed);
    assert!(a.get_mut().is_some());
    let b = a.clone();
    assert!(a.get_mut().is_none());
    drop(b);
    *a.get_mut().unwrap().reads.get_mut() = 7;
    assert_eq!(a.read(), 8);
}

#[test]
fn an_owner_keeps_the_object_past_its_last_reference() {
    // `GIT_REFCOUNT_OWN`: the repository owns its odb without a counted
    // reference of its own here, so the count may reach zero first.
    let freed = counter!();
    let owner = 0xdead_u32;
    let a = Odb::new(freed);
    let p = a.as_ptr();
    a.rc.owner
        .store(ptr::from_ref(&owner).cast_mut().cast(), Ordering::Release);
    let mut a = a;
    assert!(a.get_mut().is_none(), "owned: not sole");
    drop(a);
    assert_eq!(freed.load(Ordering::Relaxed), 0, "the owner keeps it");
    // The owner lets go: clears the owner and frees it, as
    // `git_repository__cleanup` does with a count at zero.
    // SAFETY: the owner's object, still live.
    let rc = unsafe { &(*p).rc };
    rc.owner.store(ptr::null_mut(), Ordering::Release);
    assert!(rc.count.load(Ordering::Acquire) <= 0);
    // SAFETY: nothing else holds it: what the owner's cleanup frees.
    drop(unsafe { Box::from_raw(p) });
    assert_eq!(freed.load(Ordering::Relaxed), 1);
}

#[test]
fn shared_across_threads() {
    let freed = counter!();
    let a = Odb::new(freed);
    std::thread::scope(|s| {
        for _ in 0..4 {
            let mine = a.clone();
            s.spawn(move || {
                for _ in 0..50 {
                    let more = mine.clone();
                    more.read();
                    // A C holder on this thread too.
                    let p = more.clone().into_raw();
                    // SAFETY: the reference `p` holds.
                    unsafe { c::git_odb_free(p) };
                }
            });
        }
    });
    assert_eq!(a.reads.load(Ordering::Relaxed), 200);
    drop(a);
    assert_eq!(freed.load(Ordering::Relaxed), 1);
}

// ---------------------------------------------------------------------------
// A release of the binding's own, and an allocator that can fail
// ---------------------------------------------------------------------------

static POOL_LIVE: AtomicUsize = AtomicUsize::new(0);
static POOL_FAIL: AtomicUsize = AtomicUsize::new(0);

/// An allocation family that counts its live blocks, and fails on demand:
/// the stand-in for `git__allocator`.
#[derive(Clone, Copy, Default)]
pub struct Pool;

// SAFETY: forwards to `Global`, so blocks are valid and freed once.
unsafe impl Allocator for Pool {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        if POOL_FAIL.load(Ordering::Relaxed) != 0 {
            return Err(AllocError);
        }
        let b = std::alloc::Global.allocate(layout)?;
        POOL_LIVE.fetch_add(1, Ordering::Relaxed);
        Ok(b)
    }
    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        POOL_LIVE.fetch_sub(1, Ordering::Relaxed);
        // SAFETY: a block `allocate` gave.
        unsafe { std::alloc::Global.deallocate(ptr, layout) }
    }
}

/// A `git_refdb`-like object: its release disposes a backend first.
#[repr(C)]
pub struct Refdb {
    rc: Refcount,
    backend: AtomicPtr<u32>,
}

// SAFETY: as for `Odb`; objects live in a `Box<_, Pool>`, which the
// replaced `c_release` frees after disposing the backend.
unsafe impl CCounted for Refdb {
    type Alloc = Pool;
    unsafe fn c_up_ref(this: NonNull<Self>) {
        // SAFETY: a live object.
        unsafe { rc(this) }.inc();
    }
    unsafe fn c_down_ref(this: NonNull<Self>) -> bool {
        // SAFETY: a live object.
        unsafe { rc(this) }.dec()
    }
    unsafe fn c_release(this: NonNull<Self>) {
        // SAFETY: the last reference: nothing else reaches it.
        let b = unsafe { Box::from_raw_in(this.as_ptr(), Pool) };
        let backend = b.backend.swap(ptr::null_mut(), Ordering::AcqRel);
        if !backend.is_null() {
            // SAFETY: the backend the refdb owned.
            drop(unsafe { Box::from_raw(backend) });
        }
        drop(b);
    }
}
// SAFETY: C writes `rc`, holders `backend`: atomics.
unsafe impl CShareable for Refdb {}

#[test]
fn try_new_allocates_in_the_family_and_releases_there() {
    let before = POOL_LIVE.load(Ordering::Relaxed);
    let db = CShared::try_new(Refdb {
        rc: Refcount::one(),
        backend: AtomicPtr::new(Box::into_raw(Box::new(5))),
    })
    .ok()
    .unwrap();
    assert_eq!(POOL_LIVE.load(Ordering::Relaxed), before + 1);
    let other = db.clone();
    drop(db);
    assert!(!other.backend.load(Ordering::Acquire).is_null());
    drop(other);
    assert_eq!(POOL_LIVE.load(Ordering::Relaxed), before);
}

#[test]
fn try_new_hands_the_value_back_on_failure() {
    POOL_FAIL.store(1, Ordering::Relaxed);
    let r = CShared::try_new(Refdb {
        rc: Refcount::one(),
        backend: AtomicPtr::new(ptr::null_mut()),
    });
    POOL_FAIL.store(0, Ordering::Relaxed);
    let v = r.expect_err("the allocation failed");
    assert_eq!(v.rc.count.load(Ordering::Relaxed), 1);
}

// ---------------------------------------------------------------------------
// A type C writes beyond its count: no `Deref`, reached by pointer
// ---------------------------------------------------------------------------

/// A `git_index`-like object: C writes `entries` directly, so no `&Index`
/// may be held while it is shared.
#[repr(C)]
pub struct Index {
    rc: Refcount,
    entries: usize,
}

// SAFETY: as for `Odb`; only the count is borrowed. (`entries` is written
// without synchronisation, so `Index` keeps it out of `&Index` instead of
// being `CShareable`; a real index is also not `Sync`.)
unsafe impl CCounted for Index {
    type Alloc = std::alloc::Global;
    unsafe fn c_up_ref(this: NonNull<Self>) {
        // SAFETY: a live object; only the count is borrowed.
        unsafe { rc(this) }.inc();
    }
    unsafe fn c_down_ref(this: NonNull<Self>) -> bool {
        // SAFETY: as above.
        unsafe { rc(this) }.dec()
    }
    unsafe fn c_is_sole(this: NonNull<Self>) -> bool {
        // SAFETY: as above.
        unsafe { rc(this) }.count.load(Ordering::Acquire) == 1
    }
}

/// C's `index->entries++`, through the pointer.
unsafe fn c_add_entry(ix: *mut Index) {
    // SAFETY: a live index, no reference covers it.
    unsafe { (*ix).entries += 1 };
}

#[test]
fn a_type_c_writes_is_reached_by_pointer() {
    let mut ix = CShared::from_box(Box::new(Index {
        rc: Refcount::one(),
        entries: 0,
    }));
    let other = ix.clone();
    // SAFETY: a live index, used on this thread only.
    unsafe { c_add_entry(other.as_ptr()) };
    drop(other);
    // Sole again: the exclusive borrow is sound.
    assert_eq!(ix.get_mut().map(|i| i.entries), Some(1));
}

// ---------------------------------------------------------------------------
// A repository publishing its odb: `CAtomicOwner<CShared<_>>`
// ---------------------------------------------------------------------------

#[test]
fn published_lazily_and_the_loser_released() {
    let freed = counter!();
    let slot: CAtomicOwner<CShared<Odb>> = CAtomicOwner::new();
    std::thread::scope(|s| {
        for _ in 0..4 {
            s.spawn(|| {
                let odb = slot.get_or_init(|| Odb::new(freed));
                odb.read();
                // A strong reference out of the weak one, as
                // `git_repository_odb` hands out.
                let strong = odb.clone();
                strong.read();
            });
        }
    });
    let winner = slot.get().unwrap();
    assert_eq!(winner.reads.load(Ordering::Relaxed), 8);
    // Every loser was freed, the winner is not.
    let losers = freed.load(Ordering::Relaxed);
    assert!(losers <= 3);
    drop(slot);
    assert_eq!(freed.load(Ordering::Relaxed), losers + 1);
}

/// The consumer side writes no `unsafe`.
#[forbid(unsafe_code)]
mod consumer {
    use super::*;

    #[test]
    fn safe_api() {
        let freed = counter!();
        let a = Odb::new(freed);
        let b = a.clone();
        let total: usize = [&a, &b].iter().map(|o| o.read()).sum();
        assert_eq!(total, 3);
        assert_eq!(format!("{a:p}"), format!("{b:p}"));
        drop((a, b));
        assert_eq!(freed.load(Ordering::Relaxed), 1);
    }
}

#[test]
fn layout() {
    assert_eq!(size_of::<CShared<Odb>>(), size_of::<*mut Odb>());
    assert_eq!(size_of::<Option<CShared<Odb>>>(), size_of::<*mut Odb>());
    assert_eq!(
        size_of::<CAtomicOwner<CShared<Odb>>>(),
        size_of::<*mut Odb>()
    );
}
