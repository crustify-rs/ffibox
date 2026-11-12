//! C tables implemented in Rust: `CTable` and `impl_cvtable!` over a toy
//! C table whose "C library" is the `lib` module, calling the slots through
//! the table pointer as C does.

#![allow(missing_docs)]

use core::cell::{Cell, RefCell};
use core::ffi::c_int;
use core::ptr::{self, NonNull};
use std::rc::Rc;
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};

use ffibox::vtable::CTable;
use ffibox::{CMutSeam, CRefSeam, define_ctype, impl_cvtable};

mod ffi {
    use core::ffi::c_int;

    /// A C library's key-value store interface. `version` is the library's
    /// own field, kept from the base value; `scan` takes the table last.
    #[repr(C)]
    #[derive(Default)]
    pub struct store {
        pub version: c_int,
        pub get: Option<unsafe extern "C" fn(*mut store, c_int) -> c_int>,
        pub set: Option<unsafe extern "C" fn(*mut store, c_int, c_int) -> c_int>,
        pub scan: Option<unsafe extern "C" fn(*mut c_int, *mut store) -> c_int>,
        pub free: Option<unsafe extern "C" fn(*mut store)>,
    }
}

define_ctype!(Store, StoreRef, StoreMut, ffi::store);

/// Stands in for the C library: it calls a table's slots with the table.
mod lib {
    use core::ffi::c_int;

    use super::ffi::store;

    /// # Safety
    ///
    /// `s` is a live table with its slots set.
    pub unsafe fn get(s: *mut store, k: c_int) -> c_int {
        // SAFETY: per this function's contract.
        unsafe { ((*s).get.unwrap())(s, k) }
    }

    /// # Safety
    ///
    /// As `get`.
    pub unsafe fn set(s: *mut store, k: c_int, v: c_int) -> c_int {
        // SAFETY: per this function's contract.
        unsafe { ((*s).set.unwrap())(s, k, v) }
    }

    /// # Safety
    ///
    /// As `get`.
    pub unsafe fn scan(s: *mut store) -> (c_int, c_int) {
        let mut out = 0;
        // SAFETY: per this function's contract; `out` is writable.
        let rc = unsafe { ((*s).scan.unwrap())(&raw mut out, s) };
        (rc, out)
    }

    /// # Safety
    ///
    /// `s` is a live table, released once, here.
    pub unsafe fn free(s: *mut store) {
        // SAFETY: per this function's contract.
        unsafe { ((*s).free.unwrap())(s) }
    }
}

/// What a store does, implemented in Rust.
pub trait Backend {
    fn get(&self, k: c_int) -> c_int;
    fn set(&mut self, k: c_int, v: c_int);
    fn count(&self) -> c_int;

    /// Called by the slots with the table, so a test can re-enter it.
    fn on_get(&self, _table: *mut ffi::store) {}
    fn on_set(&mut self, _table: *mut ffi::store) {}
}

const PANICKED: c_int = -100;
const BUSY: c_int = -200;

impl_cvtable! {
    impl<B: Backend> CVtable<B> for Store {
        /// `store.get` over `Backend::get`, shared.
        fn get(@table, k: c_int) -> c_int {
            on panic: PANICKED,
            on busy: BUSY,
            |b: &B| {
                b.on_get(table);
                b.get(k)
            },
        }
        /// `store.set` over `Backend::set`, exclusive.
        fn set(@table, k: c_int, v: c_int) -> c_int {
            on panic: PANICKED,
            on busy: BUSY,
            |b: &mut B| {
                b.on_set(table);
                b.set(k, v);
                0
            },
        }
        /// `store.scan`, the table last, writing an out-parameter.
        fn scan(out: *mut c_int, @table) -> c_int {
            on panic: PANICKED,
            on busy: BUSY,
            |b: &B| {
                // SAFETY: C passes a writable slot.
                unsafe { out.write(b.count()) };
                0
            },
        }
        release free;
    }
}

/// A map backend that counts its drops.
#[derive(Default)]
struct Map {
    entries: Vec<(c_int, c_int)>,
    drops: Rc<Cell<usize>>,
}

impl Map {
    fn counted() -> (Self, Rc<Cell<usize>>) {
        let map = Self::default();
        let drops = map.drops.clone();
        (map, drops)
    }
}

impl Drop for Map {
    fn drop(&mut self) {
        self.drops.set(self.drops.get() + 1);
    }
}

impl Backend for Map {
    fn get(&self, k: c_int) -> c_int {
        self.entries
            .iter()
            .find(|(key, _)| *key == k)
            .map_or(-1, |(_, v)| *v)
    }

    fn set(&mut self, k: c_int, v: c_int) {
        self.entries.retain(|(key, _)| *key != k);
        self.entries.push((k, v));
    }

    fn count(&self) -> c_int {
        c_int::try_from(self.entries.len()).unwrap()
    }
}

fn base() -> ffi::store {
    ffi::store {
        version: 7,
        ..ffi::store::default()
    }
}

#[test]
fn slots_reach_the_state_and_keep_the_base() {
    let (map, drops) = Map::counted();
    let table = CTable::<Store, _>::new(base(), map);
    let p = table.into_c().as_ptr();
    // SAFETY: `p` is a live table C holds until its release.
    unsafe {
        assert_eq!(
            (*p).version,
            7,
            "a field no slot names keeps its base value"
        );
        assert_eq!(lib::set(p, 1, 10), 0);
        assert_eq!(lib::set(p, 2, 20), 0);
        assert_eq!(lib::get(p, 2), 20);
        assert_eq!(lib::get(p, 3), -1);
        assert_eq!(
            lib::scan(p),
            (0, 2),
            "a table argument last is recovered too"
        );
        lib::free(p);
    }
    assert_eq!(drops.get(), 1);
}

/// A backend whose slots panic on request.
#[derive(Default)]
struct Panicking {
    get: bool,
    set: bool,
    calls: Cell<usize>,
    drops: Rc<Cell<usize>>,
}

impl Drop for Panicking {
    fn drop(&mut self) {
        self.drops.set(self.drops.get() + 1);
    }
}

impl Backend for Panicking {
    fn get(&self, k: c_int) -> c_int {
        self.calls.set(self.calls.get() + 1);
        assert!(!self.get, "get panics");
        k
    }

    fn set(&mut self, _: c_int, _: c_int) {
        self.calls.set(self.calls.get() + 1);
        assert!(!self.set, "set panics");
    }

    fn count(&self) -> c_int {
        0
    }
}

#[test]
#[cfg(feature = "std")]
fn a_panicking_slot_returns_its_declared_result() {
    let drops = Rc::new(Cell::new(0));
    let backend = Panicking {
        get: true,
        set: true,
        calls: Cell::new(0),
        drops: drops.clone(),
    };
    let mut table = CTable::<Store, _>::new(base(), backend);
    let p = table.as_c_ptr();
    // SAFETY: the owner's live table, lent for these calls.
    unsafe {
        assert_eq!(lib::get(p, 1), PANICKED, "a shared slot");
        assert_eq!(lib::set(p, 1, 1), PANICKED, "an exclusive slot");
    }
    // The panics left their entries: both kinds of slot enter again.
    table.state_mut().get = false;
    table.state_mut().set = false;
    // SAFETY: as above.
    unsafe {
        assert_eq!(lib::get(p, 5), 5);
        assert_eq!(lib::set(p, 1, 1), 0);
    }
    assert_eq!(table.state().unwrap().calls.get(), 4);
    drop(table);
    assert_eq!(
        drops.get(),
        1,
        "a panic does not leak or double-drop the state"
    );
}

#[test]
fn release_reclaims_exactly_once_whoever_releases() {
    // C releases a surrendered table.
    let (map, drops) = Map::counted();
    let p = CTable::<Store, _>::new(base(), map).into_c().as_ptr();
    // SAFETY: C's table, released once.
    unsafe { lib::free(p) };
    assert_eq!(drops.get(), 1);

    // The owner drops a table it kept.
    let (map, drops) = Map::counted();
    drop(CTable::<Store, _>::new(base(), map));
    assert_eq!(drops.get(), 1);

    // C releases a table it was only lent: deferred to the owner's drop.
    let (map, drops) = Map::counted();
    let mut table = CTable::<Store, _>::new(base(), map);
    // SAFETY: the owner's live table, lent; its release is deferred.
    unsafe { lib::free(table.as_c_ptr()) };
    assert_eq!(drops.get(), 0);
    // SAFETY: as above: the table is still the owner's, but released.
    assert_eq!(unsafe { lib::get(table.as_c_ptr(), 1) }, BUSY);
    assert_eq!(
        table.state_mut().count(),
        0,
        "the state outlives C's release"
    );
    drop(table);
    assert_eq!(drops.get(), 1);

    // NULL is no table.
    // SAFETY: NULL is ignored.
    unsafe { CTable::<Store, Map>::release(ptr::null_mut()) };
}

#[test]
fn into_inner_and_from_c_hand_the_allocation_over() {
    let (map, drops) = Map::counted();
    let table = CTable::<Store, _>::new(base(), map);
    // SAFETY: the owner's live table, lent.
    unsafe { lib::set(table.as_c_ptr(), 4, 40) };
    let p = table.into_c();
    // SAFETY: from `into_c`; C has not released it and will not use it.
    let table = unsafe { CTable::<Store, Map>::from_c(p) };
    let map = table.into_inner();
    assert_eq!(drops.get(), 0);
    assert_eq!(map.get(4), 40);
    drop(map);
    assert_eq!(drops.get(), 1);
}

#[test]
fn handles_designate_the_table() {
    let mut table = CTable::<Store, _>::new(base(), Map::default());
    let p = table.as_c_ptr();
    assert_eq!(table.as_ref().as_ptr(), p.cast_const());
    assert_eq!(table.as_mut().as_mut_ptr(), p);
    assert_eq!(table.into_c().as_ptr(), p);
    // SAFETY: C's table, released once.
    unsafe { lib::free(p) };
}

/// A backend whose slots re-enter their own table through C.
#[derive(Default)]
struct Reentrant {
    /// What `get` does with the table: 0 nothing, 2 set, 3 free and get.
    from_get: c_int,
    /// What `set` does with the table, as `from_get`.
    from_set: c_int,
    seen: RefCell<Vec<c_int>>,
    drops: Rc<Cell<usize>>,
}

impl Reentrant {
    fn reenter(&self, what: c_int, table: *mut ffi::store) {
        // SAFETY: the table C passed the running slot, live while it runs.
        let rc = unsafe {
            match what {
                2 => lib::set(table, 0, 0),
                3 => {
                    lib::free(table);
                    lib::get(table, 0)
                }
                _ => return,
            }
        };
        self.seen.borrow_mut().push(rc);
    }
}

impl Drop for Reentrant {
    fn drop(&mut self) {
        self.drops.set(self.drops.get() + 1);
    }
}

impl Backend for Reentrant {
    fn get(&self, _: c_int) -> c_int {
        1
    }

    fn set(&mut self, _: c_int, _: c_int) {}

    fn count(&self) -> c_int {
        0
    }

    fn on_get(&self, table: *mut ffi::store) {
        self.reenter(self.from_get, table);
    }

    fn on_set(&mut self, table: *mut ffi::store) {
        let what = core::mem::replace(&mut self.from_set, 0);
        self.reenter(what, table);
    }
}

fn reentrant(from_get: c_int, from_set: c_int) -> (CTable<Store, Reentrant>, Rc<Cell<usize>>) {
    let backend = Reentrant {
        from_get,
        from_set,
        seen: RefCell::new(Vec::new()),
        drops: Rc::new(Cell::new(0)),
    };
    let drops = backend.drops.clone();
    (CTable::new(base(), backend), drops)
}

/// Records each nesting level's result: a shared slot re-entered from a
/// shared one runs, from an exclusive one it is refused.
#[test]
fn reentry_follows_each_slots_receiver() {
    struct Nest {
        depth: Cell<u32>,
        inner: RefCell<Vec<c_int>>,
        from_set: Cell<c_int>,
    }
    impl Backend for Nest {
        fn get(&self, _: c_int) -> c_int {
            1
        }
        fn set(&mut self, _: c_int, _: c_int) {}
        fn count(&self) -> c_int {
            0
        }
        fn on_get(&self, table: *mut ffi::store) {
            if self.depth.replace(self.depth.get() + 1) == 0 {
                // SAFETY: the running slot's live table.
                let rc = unsafe { lib::get(table, 0) };
                self.inner.borrow_mut().push(rc);
                // SAFETY: as above.
                let rc = unsafe { lib::set(table, 0, 0) };
                self.inner.borrow_mut().push(rc);
            }
        }
        fn on_set(&mut self, table: *mut ffi::store) {
            let what = self.from_set.replace(0);
            // SAFETY: the running slot's live table.
            let rc = unsafe {
                match what {
                    1 => lib::get(table, 0),
                    2 => lib::set(table, 0, 0),
                    _ => return,
                }
            };
            self.inner.borrow_mut().push(rc);
        }
    }
    let nest = Nest {
        depth: Cell::new(0),
        inner: RefCell::new(Vec::new()),
        from_set: Cell::new(0),
    };
    let mut table = CTable::<Store, _>::new(base(), nest);
    let p = table.as_c_ptr();
    // SAFETY: the owner's live table, lent for the call.
    assert_eq!(unsafe { lib::get(p, 0) }, 1);
    assert_eq!(
        table.state_mut().inner.take(),
        [1, BUSY],
        "inside a shared slot: shared runs, exclusive is refused"
    );

    table.state_mut().from_set.set(1);
    // SAFETY: as above.
    assert_eq!(unsafe { lib::set(p, 0, 0) }, 0);
    table.state_mut().from_set.set(2);
    // SAFETY: as above.
    assert_eq!(unsafe { lib::set(p, 0, 0) }, 0);
    assert_eq!(
        table.state_mut().inner.take(),
        [BUSY, BUSY],
        "inside an exclusive slot: every slot is refused"
    );
}

#[test]
fn a_release_inside_a_slot_waits_for_it() {
    // From an exclusive slot.
    let (table, drops) = reentrant(0, 3);
    let p = table.into_c().as_ptr();
    // SAFETY: C's live table; the slot releases it and then exits.
    assert_eq!(unsafe { lib::set(p, 0, 0) }, 0);
    assert_eq!(drops.get(), 1, "dropped once, when the slot exited");

    // From a shared slot.
    let (table, drops) = reentrant(3, 0);
    let p = table.into_c().as_ptr();
    // SAFETY: as above.
    assert_eq!(unsafe { lib::get(p, 0) }, 1);
    assert_eq!(drops.get(), 1);
}

#[test]
fn a_slot_after_a_release_inside_a_slot_is_refused() {
    struct Releasing {
        after: Rc<Cell<c_int>>,
    }
    impl Backend for Releasing {
        fn get(&self, _: c_int) -> c_int {
            1
        }
        fn set(&mut self, _: c_int, _: c_int) {}
        fn count(&self) -> c_int {
            0
        }
        fn on_set(&mut self, table: *mut ffi::store) {
            // SAFETY: the running slot's live table: released here, its
            // allocation kept until this slot exits.
            let rc = unsafe {
                lib::free(table);
                lib::get(table, 0)
            };
            self.after.set(rc);
        }
    }
    let after = Rc::new(Cell::new(0));
    let backend = Releasing {
        after: after.clone(),
    };
    let p = CTable::<Store, _>::new(base(), backend).into_c().as_ptr();
    // SAFETY: C's live table.
    assert_eq!(unsafe { lib::set(p, 0, 0) }, 0);
    assert_eq!(after.get(), BUSY);
}

#[test]
fn the_owners_shared_state_refuses_exclusive_slots_until_borrowed_mutably() {
    let mut table = CTable::<Store, _>::new(base(), Map::default());
    let p = table.as_c_ptr();
    assert_eq!(table.state().unwrap().count(), 0);
    // SAFETY: the owner's live table, lent for each call.
    unsafe {
        assert_eq!(lib::set(p, 1, 1), BUSY, "the owner may still hold `&S`");
        assert_eq!(lib::get(p, 1), -1, "shared slots still run");
    }
    let _ = table.as_mut();
    // SAFETY: as above.
    assert_eq!(unsafe { lib::set(p, 1, 1) }, 0);
    let _ = table.state();
    let _ = table.state_mut();
    // SAFETY: as above.
    assert_eq!(unsafe { lib::set(p, 2, 2) }, 0);
    assert_eq!(table.into_inner().count(), 2);
}

thread_local! {
    static OWNER: Cell<*const CTable<Store, Peek>> = const { Cell::new(ptr::null()) };
}

/// A backend that asks its owner for the state from inside a slot.
struct Peek {
    seen: Cell<Option<bool>>,
}

impl Backend for Peek {
    fn get(&self, _: c_int) -> c_int {
        0
    }
    fn set(&mut self, _: c_int, _: c_int) {}
    fn count(&self) -> c_int {
        0
    }
    fn on_get(&self, _: *mut ffi::store) {
        self.peek();
    }
    fn on_set(&mut self, _: *mut ffi::store) {
        // `&mut self` would alias any `&Self` the owner lent: it must refuse.
        let owner = OWNER.get();
        // SAFETY: the owner, borrowed shared by the test for the C call.
        let lent = unsafe { (*owner).state() }.is_some();
        self.seen.set(Some(lent));
    }
}

impl Peek {
    fn peek(&self) {
        let owner = OWNER.get();
        // SAFETY: as in `on_set`.
        let lent = unsafe { (*owner).state() }.is_some();
        self.seen.set(Some(lent));
    }
}

#[test]
fn the_owners_shared_state_is_refused_inside_an_exclusive_slot() {
    let peek = Peek {
        seen: Cell::new(None),
    };
    let mut table = CTable::<Store, _>::new(base(), peek);
    let p = table.as_c_ptr();
    // The owner, lent through `&table` for the call; set again after each
    // `&mut table`, which ends the shared borrow.
    OWNER.set(&raw const table);
    // SAFETY: the owner's live table, lent for the call.
    assert_eq!(unsafe { lib::set(p, 0, 0) }, 0);
    assert_eq!(table.state_mut().seen.take(), Some(false));
    OWNER.set(&raw const table);
    // SAFETY: as above.
    assert_eq!(unsafe { lib::get(p, 0) }, 0);
    assert_eq!(
        table.state_mut().seen.take(),
        Some(true),
        "inside a shared one, lent"
    );
    OWNER.set(ptr::null());
}

/// A thread-safe backend, for slots called from several threads.
#[derive(Default)]
struct Atomic {
    value: AtomicI32,
    gets: AtomicUsize,
}

impl Backend for Atomic {
    fn get(&self, _: c_int) -> c_int {
        self.gets.fetch_add(1, Ordering::Relaxed);
        self.value.load(Ordering::Relaxed)
    }
    fn set(&mut self, _: c_int, v: c_int) {
        *self.value.get_mut() = v;
    }
    fn count(&self) -> c_int {
        1
    }
}

// SAFETY: a `ffi::store` is plain data and function pointers, read-only once
// built; its slots are the trampolines above, which synchronise through the
// table's entry state.
unsafe impl Send for Store {}
// SAFETY: as above.
unsafe impl Sync for Store {}

#[test]
fn slots_run_from_several_threads() {
    let mut table = CTable::<Store, _>::new(base(), Atomic::default());
    // SAFETY: the owner's live table, lent.
    assert_eq!(unsafe { lib::set(table.as_c_ptr(), 0, 5) }, 0);
    let threads = if cfg!(miri) { 2 } else { 8 };
    let rounds = if cfg!(miri) { 4 } else { 1000 };
    let ran = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..threads {
            let (table, ran) = (&table, &ran);
            scope.spawn(move || {
                // Lent through `&table` for the scope.
                let p = table.as_c_ptr();
                for _ in 0..rounds {
                    // SAFETY: the owner's live table, lent; `Atomic` is `Sync`.
                    match unsafe { lib::get(p, 0) } {
                        5 => _ = ran.fetch_add(1, Ordering::Relaxed),
                        // Refused while another thread's `set` runs.
                        rc => assert_eq!(rc, BUSY),
                    }
                    // SAFETY: as above; an exclusive slot runs alone, or is
                    // refused while another slot runs.
                    let rc = unsafe { lib::set(p, 0, 5) };
                    assert!(rc == 0 || rc == BUSY, "{rc}");
                }
            });
        }
    });
    assert_eq!(
        table.state_mut().gets.load(Ordering::Relaxed),
        ran.into_inner(),
        "every get that was not refused reached the state"
    );
}

/// A slot the macro does not express, written by hand on the same terms.
mod by_hand {
    use core::ffi::c_int;

    use ffibox::define_ctype;
    use ffibox::vtable::{CTable, CVtable};

    #[repr(C)]
    #[derive(Default)]
    pub struct counter {
        pub bump: Option<unsafe extern "C" fn(*mut counter) -> c_int>,
        pub free: Option<unsafe extern "C" fn(*mut counter)>,
    }

    define_ctype!(Counter, CounterRef, CounterMut, counter);

    unsafe extern "C" fn bump(p: *mut counter) -> c_int {
        // SAFETY: C calls the slot with the table it read it from, live.
        unsafe {
            CTable::<Counter, c_int>::enter(
                p,
                || -1,
                || -2,
                |n: &mut c_int| {
                    *n += 1;
                    *n
                },
            )
        }
    }

    // SAFETY: `bump` reaches the state only through `CTable::enter` on the
    // table C passes it, and `free` is `CTable::release`.
    unsafe impl CVtable<c_int> for Counter {
        fn install(table: &mut counter) {
            table.bump = Some(bump);
            table.free = Some(CTable::<Counter, c_int>::release);
        }
    }
}

#[test]
fn a_table_written_by_hand() {
    let p = CTable::<by_hand::Counter, c_int>::new(by_hand::counter::default(), 0)
        .into_c()
        .as_ptr();
    // SAFETY: C's live table, released once.
    unsafe {
        assert_eq!(((*p).bump.unwrap())(p), 1);
        assert_eq!(((*p).bump.unwrap())(p), 2);
        ((*p).free.unwrap())(p);
    }
}

#[test]
fn the_owner_follows_the_states_threads() {
    fn send<T: Send>() {}
    fn sync<T: Sync>() {}
    send::<CTable<Store, Atomic>>();
    sync::<CTable<Store, Atomic>>();
}

#[test]
fn a_surrendered_table_is_adopted_by_its_owner_type() {
    let p: NonNull<ffi::store> = CTable::<Store, _>::new(base(), Map::default()).into_c();
    // SAFETY: from `into_c`, unreleased and unused by C.
    let table = unsafe { CTable::<Store, Map>::from_c(p) };
    drop(table);
}
