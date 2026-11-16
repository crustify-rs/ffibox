//! `CShared` over Rust objects whose count is their own field, which C takes
//! and drops too: libgit2's `git_refcount { refcount; owner; }`, with an
//! owner that keeps the object alive past its last counted reference, a
//! release that frees through another allocation family, a type C writes
//! beyond its count (no `Deref`), references taken and dropped from several
//! threads and through the C-side routines, a count reset on adoption, a
//! full count that aborts rather than wraps, and panics in teardown.

#![allow(missing_docs)]

use core::alloc::{AllocError, Allocator, Layout};
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use ffibox::{CAtomicOwner, CCounted, CShareable, CShared};

// ---------------------------------------------------------------------------
// `git_refcount`, and objects that start with one
// ---------------------------------------------------------------------------

/// `git_refcount` in a module of its own, so that its fields are private to
/// it: safe code anywhere else (this file's tests included) can make a count
/// only at one reference with no owner (`new`), and change it only through
/// `unsafe` methods, as `CCounted`'s contract asks of a type with a
/// `c_is_sole_owner`.
mod rc {
    use core::ffi::c_void;
    use core::ptr;
    use core::sync::atomic::{AtomicI32, AtomicPtr, Ordering, fence};

    /// The count and the owner, both atomic; no `Default`, no `Clone`.
    #[repr(C)]
    pub struct Refcount {
        count: AtomicI32,
        owner: AtomicPtr<c_void>,
    }

    /// The highest count `inc` takes, as `Arc`'s `MAX_REFCOUNT`; a test
    /// lowers it to reach it.
    pub static MAX: AtomicI32 = AtomicI32::new(i32::MAX / 2);

    impl Refcount {
        /// One reference, no owner: the only count safe code can make.
        pub const fn new() -> Self {
            Refcount {
                count: AtomicI32::new(1),
                owner: AtomicPtr::new(ptr::null_mut()),
            }
        }
        /// The count, for assertions.
        pub fn count(&self) -> i32 {
            self.count.load(Ordering::Acquire)
        }
        /// Overwrite the count: what C, or a broken constructor, might do.
        ///
        /// # Safety
        ///
        /// The object is adopted afterwards by `CShared::from_box` (whose
        /// `c_init` resets it) and is not reached through `get_mut` first.
        pub unsafe fn set_count(&self, n: i32) {
            self.count.store(n, Ordering::Relaxed);
        }
        /// `GIT_REFCOUNT_OWN`.
        ///
        /// # Safety
        ///
        /// `owner` is NULL or keeps the object alive until it sets NULL
        /// again, and releases it then if the count is spent.
        pub unsafe fn own(&self, owner: *mut c_void) {
            self.owner.store(owner, Ordering::Release);
        }

        // The `CCounted` operations take the count's pointer and borrow
        // only the atomic each one uses: `Refcount` has padding between
        // its fields, and a `&self` would stay protected across the
        // decrement while another thread takes `&mut` of the object
        // (`CCounted`'s contract; Miri's many-seeds run found it).

        /// The count field, borrowed alone.
        ///
        /// # Safety
        ///
        /// `this` is live for `'a`.
        unsafe fn count_of<'a>(this: *const Refcount) -> &'a AtomicI32 {
            // SAFETY: the caller's contract; the projection forms no
            // reference to the rest.
            unsafe { &(*this).count }
        }
        /// The owner field, borrowed alone.
        ///
        /// # Safety
        ///
        /// As `count_of`.
        unsafe fn owner_of<'a>(this: *const Refcount) -> &'a AtomicPtr<c_void> {
            // SAFETY: as in `count_of`.
            unsafe { &(*this).owner }
        }
        /// `c_init`.
        ///
        /// # Safety
        ///
        /// Nothing else reaches the object.
        pub unsafe fn init(this: *const Refcount) {
            // SAFETY: the caller's contract.
            unsafe { Self::count_of(this) }.store(1, Ordering::Relaxed);
            // SAFETY: as above.
            unsafe { Self::owner_of(this) }.store(ptr::null_mut(), Ordering::Relaxed);
        }
        /// `GIT_REFCOUNT_INC`, refusing before the count could wrap.
        ///
        /// # Safety
        ///
        /// The caller holds a reference.
        pub unsafe fn inc(this: *const Refcount) -> bool {
            // SAFETY: live: the caller's reference.
            let count = unsafe { Self::count_of(this) };
            let max = MAX.load(Ordering::Relaxed);
            let mut n = count.load(Ordering::Relaxed);
            while n < max {
                match count.compare_exchange_weak(n, n + 1, Ordering::Relaxed, Ordering::Relaxed) {
                    Ok(_) => return true,
                    Err(now) => n = now,
                }
            }
            false
        }
        /// C's own increment, `SEQ_CST`, unchecked.
        ///
        /// # Safety
        ///
        /// The caller holds a reference.
        pub unsafe fn c_inc(this: *const Refcount) {
            // SAFETY: live: the caller's reference.
            unsafe { Self::count_of(this) }.fetch_add(1, Ordering::SeqCst);
        }
        /// `GIT_REFCOUNT_DEC` without the free: `Release`, then `Acquire`
        /// before it answers "free now"; the owner read after the decrement,
        /// only by the last one out.
        ///
        /// # Safety
        ///
        /// The caller gives up a reference it holds.
        pub unsafe fn dec(this: *const Refcount) -> bool {
            // SAFETY: live until the decrement; only the atomic is borrowed.
            if unsafe { Self::count_of(this) }.fetch_sub(1, Ordering::Release) != 1 {
                return false;
            }
            fence(Ordering::Acquire);
            // SAFETY: the count is spent: nothing but an owner reaches the
            // object, and an owner keeps it alive while it is set.
            unsafe { Self::owner_of(this) }
                .load(Ordering::Acquire)
                .is_null()
        }
        /// One reference, no owner, read with `Acquire`.
        ///
        /// # Safety
        ///
        /// The caller holds a reference.
        pub unsafe fn sole(this: *const Refcount) -> bool {
            // SAFETY: live: the caller's reference.
            let (count, owner) = unsafe { (Self::count_of(this), Self::owner_of(this)) };
            count.load(Ordering::Acquire) == 1 && owner.load(Ordering::Acquire).is_null()
        }
    }
}
use rc::{MAX, Refcount};

/// The count of the object at `this`, as a pointer: every counted type
/// here starts with its `Refcount` (`#[repr(C)]`).
fn rc<T>(this: NonNull<T>) -> *const Refcount {
    this.as_ptr().cast_const().cast()
}

/// The `CCounted` methods over a leading `Refcount`.
macro_rules! counted_by_refcount {
    () => {
        unsafe fn c_init(this: NonNull<Self>) {
            // SAFETY: the caller has the object to itself.
            unsafe { Refcount::init(rc(this)) }
        }
        unsafe fn c_up_ref(this: NonNull<Self>) -> bool {
            // SAFETY: a live object (the caller's reference).
            unsafe { Refcount::inc(rc(this)) }
        }
        unsafe fn c_down_ref(this: NonNull<Self>) -> bool {
            // SAFETY: as above.
            unsafe { Refcount::dec(rc(this)) }
        }
        unsafe fn c_is_sole_owner(this: NonNull<Self>) -> bool {
            // SAFETY: as above.
            unsafe { Refcount::sole(rc(this)) }
        }
    };
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

// SAFETY: `rc` counts the references as `GIT_REFCOUNT_*` do, atomically,
// refusing before it wraps, with `Arc`'s orderings; an object with an owner
// is released by the owner; safe code cannot change `rc`; objects live in a
// `Box` of `Global`.
unsafe impl CCounted for Odb {
    type Alloc = std::alloc::Global;
    counted_by_refcount!();
}
// SAFETY: C writes `rc` only, and holders `reads`, both atomic.
unsafe impl CShareable for Odb {}

impl Odb {
    fn new(freed: &'static AtomicUsize) -> CShared<Odb> {
        CShared::from_box(Box::new(Odb {
            rc: Refcount::new(),
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
        // A real export runs this in its panic barrier.
        // SAFETY: C passes NULL or a reference it gives up.
        unsafe { CShared::release(db) }
    }

    /// What C's inline macro does to the field directly (`SEQ_CST`).
    pub unsafe fn refcount_inc(db: *mut Odb) {
        // SAFETY: C's caller holds a reference.
        unsafe { Refcount::c_inc(&raw const (*db).rc) };
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
    let p = CShared::into_raw(a.clone());
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
    let lent = unsafe { CShared::from_borrowed(CShared::as_ptr(&a)) }.unwrap();
    drop(a);
    assert_eq!(lent.read(), 1);
    drop(lent);
    assert_eq!(freed.load(Ordering::Relaxed), 1);
}

#[test]
fn increment_and_decrement_count_as_arc() {
    let freed = counter!();
    let a = Odb::new(freed);
    let p = CShared::into_raw(a);
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
    assert!(CShared::get_mut(&mut a).is_some());
    let b = a.clone();
    assert!(CShared::get_mut(&mut a).is_none());
    drop(b);
    *CShared::get_mut(&mut a).unwrap().reads.get_mut() = 7;
    assert_eq!(a.read(), 8);
}

#[test]
fn adoption_sets_the_count_whatever_the_value_carried() {
    // A value whose count says 0 (a derived `Default` would) or 5: `from_box`
    // resets it to one reference, so a clone and a drop leave it alive.
    let freed = counter!();
    for start in [0, 5, -3] {
        let v = Odb {
            rc: Refcount::new(),
            reads: AtomicUsize::new(0),
            freed,
        };
        // SAFETY: `from_box` adopts `v` next.
        unsafe { v.rc.set_count(start) };
        let a = CShared::from_box(Box::new(v));
        drop(a.clone());
        assert_eq!(a.read(), 1, "still alive after a clone's drop");
        drop(a);
    }
    assert_eq!(freed.load(Ordering::Relaxed), 3);
}

#[test]
fn a_value_stored_through_get_mut_keeps_the_object_counted() {
    // N1 (review r2): `get_mut` lets safe code store any `Odb` it can make.
    // Every count safe code can make is one reference, so the stored value
    // still counts `a`'s reference: a clone and its drop free nothing.
    let freed = counter!();
    let mut a = Odb::new(freed);
    let fresh = Odb {
        rc: Refcount::new(),
        reads: AtomicUsize::new(0),
        freed,
    };
    let old = core::mem::replace(CShared::get_mut(&mut a).unwrap(), fresh);
    drop(old); // a plain value: its `Drop` counts, nothing is deallocated
    assert_eq!(freed.load(Ordering::Relaxed), 1);
    drop(a.clone());
    assert_eq!(a.read(), 1, "still alive after a clone's drop");
    let mut b = Odb::new(freed);
    core::mem::swap(
        CShared::get_mut(&mut a).unwrap(),
        CShared::get_mut(&mut b).unwrap(),
    );
    drop(b.clone());
    drop(a);
    assert_eq!(b.read(), 2, "`b` holds `a`'s value now, read once before");
    drop(b);
    assert_eq!(freed.load(Ordering::Relaxed), 3);
}

#[test]
fn an_owner_keeps_the_object_past_its_last_reference() {
    // `GIT_REFCOUNT_OWN`: the repository owns its odb without a counted
    // reference of its own here, so the count may reach zero first.
    let freed = counter!();
    let owner = 0xdead_u32;
    let mut a = Odb::new(freed);
    let p = CShared::as_ptr(&a);
    // SAFETY: `owner` outlives the object here, and releases it below.
    unsafe { a.rc.own(ptr::from_ref(&owner).cast_mut().cast()) };
    assert!(CShared::get_mut(&mut a).is_none(), "owned: not sole");
    drop(a);
    assert_eq!(freed.load(Ordering::Relaxed), 0, "the owner keeps it");
    // The owner lets go: clears the owner and frees it, as
    // `git_repository__cleanup` does with a count at zero.
    // SAFETY: the owner's object, still live.
    let rc = unsafe { &(*p).rc };
    // SAFETY: the owner lets go, and frees the spent object next.
    unsafe { rc.own(ptr::null_mut()) };
    assert_eq!(rc.count(), 0);
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
                    let p = CShared::into_raw(more.clone());
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

#[test]
fn the_last_drop_on_another_thread_happens_before_the_free_and_get_mut() {
    // Arc's orderings: a holder's last use on another thread, then its drop,
    // happen-before the free on this one, and before `get_mut`'s `&mut`.
    // Miri's race detector checks it (run with several seeds).
    let freed = counter!();
    let mut a = Odb::new(freed);
    let b = a.clone();
    let t = std::thread::spawn(move || {
        b.read();
        drop(b);
    });
    while CShared::get_mut(&mut a).is_none() {
        std::thread::yield_now();
    }
    *CShared::get_mut(&mut a).unwrap().reads.get_mut() += 1;
    t.join().unwrap();
    let c = a.clone();
    let t = std::thread::spawn(move || drop(c));
    drop(a);
    t.join().unwrap();
    assert_eq!(freed.load(Ordering::Relaxed), 1);
}

#[test]
#[cfg_attr(miri, ignore = "spawns a process")]
fn a_full_count_aborts_rather_than_wraps() {
    // In a child process: lower the bound, leak clones up to it, and clone
    // once more. `Clone` must abort, as `Arc`'s does.
    if std::env::var_os("FFIBOX_FULL_COUNT_CHILD").is_some() {
        MAX.store(8, Ordering::Relaxed);
        let a = Odb::new(counter!());
        for _ in 0..7 {
            core::mem::forget(a.clone());
        }
        let _ = a.clone();
        unreachable!("the clone past the bound aborts");
    }
    let me = std::env::current_exe().unwrap();
    let out = std::process::Command::new(me)
        .args([
            "--exact",
            "a_full_count_aborts_rather_than_wraps",
            "--nocapture",
        ])
        .env("FFIBOX_FULL_COUNT_CHILD", "1")
        .output()
        .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(out.status.signal(), Some(6), "SIGABRT: {out:?}");
    }
    assert!(!out.status.success());
}

// ---------------------------------------------------------------------------
// Panics in teardown
// ---------------------------------------------------------------------------

/// An object whose `Drop` panics once its flag is set.
#[repr(C)]
pub struct Fragile {
    rc: Refcount,
    panic: bool,
    freed: &'static AtomicUsize,
}

impl Drop for Fragile {
    fn drop(&mut self) {
        self.freed.fetch_add(1, Ordering::Relaxed);
        assert!(!self.panic, "teardown panics");
    }
}

// SAFETY: as for `Odb`.
unsafe impl CCounted for Fragile {
    type Alloc = std::alloc::Global;
    counted_by_refcount!();
}

#[test]
fn a_panicking_drop_frees_once_and_unwinds_to_the_holder() {
    let freed = counter!();
    let a = CShared::from_box(Box::new(Fragile {
        rc: Refcount::new(),
        panic: true,
        freed,
    }));
    let b = a.clone();
    drop(a);
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(b)));
    assert!(r.is_err(), "the panic reaches the last holder");
    // The box was deallocated while unwinding: the drop ran once, and
    // nothing is left to free (Miri checks for a leak or a double free).
    assert_eq!(freed.load(Ordering::Relaxed), 1);
}

#[test]
fn a_panicking_release_from_c_is_caught_by_the_export_barrier() {
    let freed = counter!();
    let a = CShared::from_box(Box::new(Fragile {
        rc: Refcount::new(),
        panic: true,
        freed,
    }));
    let p = CShared::into_raw(a);
    // The export: `CShared::release` inside the barrier.
    let status = ffibox::callback::barrier(
        // SAFETY: C's last reference.
        || unsafe {
            CShared::release(p);
            0
        },
        || -1,
    );
    assert_eq!(status, -1);
    assert_eq!(freed.load(Ordering::Relaxed), 1);
}

// ---------------------------------------------------------------------------
// A release of the binding's own, and an allocator that can fail
// ---------------------------------------------------------------------------

static POOL_LIVE: AtomicUsize = AtomicUsize::new(0);
static POOL_FAIL: AtomicUsize = AtomicUsize::new(0);

/// An allocation family that counts its live blocks, and fails on demand:
/// the stand-in for `git__allocator`. Zero-sized and stateless.
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
    counted_by_refcount!();
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
        rc: Refcount::new(),
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
        rc: Refcount::new(),
        backend: AtomicPtr::new(ptr::null_mut()),
    });
    POOL_FAIL.store(0, Ordering::Relaxed);
    let v = r.expect_err("the allocation failed");
    assert!(v.backend.load(Ordering::Relaxed).is_null());
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
// being `CShareable`.)
unsafe impl CCounted for Index {
    type Alloc = std::alloc::Global;
    counted_by_refcount!();
}

/// C's `index->entries++`, through the pointer.
unsafe fn c_add_entry(ix: *mut Index) {
    // SAFETY: a live index, no reference covers it.
    unsafe { (*ix).entries += 1 };
}

#[test]
fn a_type_c_writes_is_reached_by_pointer() {
    let mut ix = CShared::from_box(Box::new(Index {
        rc: Refcount::new(),
        entries: 0,
    }));
    let other = ix.clone();
    // SAFETY: a live index, used on this thread only.
    unsafe { c_add_entry(CShared::as_ptr(&other)) };
    drop(other);
    // Sole again: the exclusive borrow is sound.
    assert_eq!(CShared::get_mut(&mut ix).map(|i| i.entries), Some(1));
}

// ---------------------------------------------------------------------------
// `Arc`'s traits through `T`
// ---------------------------------------------------------------------------

/// A shareable value with order, hash and display.
#[repr(C)]
pub struct Name {
    rc: Refcount,
    name: &'static str,
}
impl PartialEq for Name {
    fn eq(&self, o: &Self) -> bool {
        self.name == o.name
    }
}
impl Eq for Name {}
impl PartialOrd for Name {
    fn partial_cmp(&self, o: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Name {
    fn cmp(&self, o: &Self) -> core::cmp::Ordering {
        self.name.cmp(o.name)
    }
}
impl core::hash::Hash for Name {
    fn hash<H: core::hash::Hasher>(&self, h: &mut H) {
        self.name.hash(h);
    }
}
impl core::fmt::Display for Name {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.name)
    }
}
// SAFETY: as for `Odb`.
unsafe impl CCounted for Name {
    type Alloc = std::alloc::Global;
    counted_by_refcount!();
}
// SAFETY: `name` is fixed; `rc` is atomic.
unsafe impl CShareable for Name {}

fn name(n: &'static str) -> CShared<Name> {
    CShared::from(Box::new(Name {
        rc: Refcount::new(),
        name: n,
    }))
}

#[test]
fn compares_hashes_and_displays_as_t() {
    use std::collections::{BTreeSet, HashSet};
    let (a, b, a2) = (name("a"), name("b"), name("a"));
    assert!(a == a2 && a != b && a < b);
    assert!(!CShared::ptr_eq(&a, &a2));
    assert_eq!(format!("{a}"), "a");
    let set: HashSet<_> = [a.clone(), a2.clone(), b.clone()].into_iter().collect();
    assert_eq!(set.len(), 2);
    let tree: BTreeSet<_> = [b, a].into_iter().collect();
    assert_eq!(tree.iter().map(|n| n.name).collect::<Vec<_>>(), ["a", "b"]);
    let borrowed: &Name = core::borrow::Borrow::borrow(&a2);
    assert_eq!(borrowed.name, "a");
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
