//! C tables implemented in Rust: a struct of function pointers a C library
//! calls into, each slot passed a pointer to the table, over state Rust owns.
//!
//! [`CTable<T, S>`] holds the state `S` behind the C table of the layout type
//! `T`, allocated together with the table first, so the pointer C passes a
//! slot is the pointer to the allocation. It is `Box<dyn Trait>` handed to C,
//! with the table of functions embedded rather than pointed to.
//! [`impl_cvtable!`](crate::impl_cvtable) mints the slots, one `extern "C"`
//! trampoline each, and the [`CSlots<S>`] impl that installs them, beside
//! the binding's `unsafe impl` of [`CVtable<S>`], which states that they keep
//! C's contract:
//!
//! ```
//! use core::ffi::c_int;
//! use ffibox::define_ctype;
//! use ffibox::vtable::CTable;
//!
//! mod ffi {
//!     use core::ffi::c_int;
//!     /// A C library's counter interface: `get`, `add`, and `free`.
//!     #[repr(C)]
//!     #[derive(Default)]
//!     pub struct counter {
//!         pub get: Option<unsafe extern "C" fn(*mut counter) -> c_int>,
//!         pub add: Option<unsafe extern "C" fn(*mut counter, c_int) -> c_int>,
//!         pub free: Option<unsafe extern "C" fn(*mut counter)>,
//!     }
//! }
//!
//! define_ctype!(Counter, CounterRef, CounterMut, ffi::counter);
//!
//! /// What a counter does, implemented in Rust.
//! pub trait Count {
//!     fn get(&self) -> c_int;
//!     fn add(&mut self, n: c_int);
//! }
//!
//! ffibox::impl_cvtable! {
//!     // SAFETY: the library passes each slot its counter, first, and calls
//!     // `get`, `add` and `free` on every counter.
//!     unsafe impl<C: Count> CVtable<C> for Counter {}
//!     base = ffi::counter::default();
//!     fn get(@counter) -> c_int {
//!         on panic: -1,
//!         on busy: -2,
//!         |c: &C| c.get(),
//!     }
//!     fn add(@counter, n: c_int) -> c_int {
//!         on panic: -1,
//!         on busy: -2,
//!         |c: &mut C| {
//!             c.add(n);
//!             0
//!         },
//!     }
//!     release free;
//! }
//!
//! struct Total(c_int);
//! impl Count for Total {
//!     fn get(&self) -> c_int {
//!         self.0
//!     }
//!     fn add(&mut self, n: c_int) {
//!         self.0 += n;
//!     }
//! }
//!
//! let table = CTable::<Counter, _>::new(Total(1));
//! let p = table.into_c().as_ptr();
//! // Stands in for the C library, which calls the slots with the table and
//! // frees it once.
//! // SAFETY: `p` is a live table, which C uses until its `free`.
//! unsafe {
//!     assert_eq!(((*p).add.unwrap())(p, 2), 0);
//!     assert_eq!(((*p).get.unwrap())(p), 3);
//!     ((*p).free.unwrap())(p);
//! }
//! ```
//!
//! **Receivers.** A slot declares its receiver by its body's argument type:
//! `&S` for a shared slot, `&mut S` for an exclusive one. The table's entry
//! state is a non-blocking read-write lock over `S`, as
//! `RwLock::try_read` / `try_write`: shared slots run together, re-entrant or
//! on several threads, and an exclusive slot runs only when no other slot
//! does. A slot that cannot enter is **refused** and returns its `on busy`
//! value without reaching `S`.
//!
//! **Panics.** Every slot runs its body in [`barrier`](crate::callback::barrier)
//! and returns its `on panic` value for a panic; without `std`, a panic aborts
//! at the `extern "C"` boundary.
//!
//! **Release.** The slot declared `release` is [`CTable::release`]. A release
//! while a slot runs is deferred until the last one exits, and one while the
//! owner holds the table until the owner drops it: the allocation is dropped
//! exactly once.
//!
//! [docs/vtables.md](https://github.com/crustify-rs/ffibox/blob/main/docs/vtables.md)
//! gives the design and what the binding still states.

use core::fmt;
use core::marker::PhantomData;
use core::mem::ManuallyDrop;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering};

use alloc::boxed::Box;

use crate::CCell;

/// The layout type `Self` of a C table whose slots are implemented over the
/// state `S`: the binding's word that the table [`CSlots`] builds keeps C's
/// contract. Has no items: the `unsafe impl` is the statement.
///
/// [`impl_cvtable!`](crate::impl_cvtable) takes it written out in its input,
/// `unsafe impl<…> CVtable<S> for T {}`, and emits it with the binding's own
/// `unsafe` and braces, so `#![forbid(unsafe_code)]` rejects the invocation
/// and clippy's `undocumented_unsafe_blocks` asks for its `SAFETY` comment.
/// A table written by hand implements [`CSlots`] and this trait.
///
/// # Safety
///
/// - C passes each slot [`CSlots::install`] sets its table in the argument
///   the slot's trampoline recovers it from: types check that only where no
///   other argument has the table's type (`cmp(ops *self, ops *other)` is one
///   where they cannot).
/// - Every slot `install` sets is a function that, called by C as C's
///   declaration of the slot states, reaches the state only through
///   [`CTable::<Self, S>::enter`](CTable::enter) on that argument, releases
///   the table only through [`CTable::<Self, S>::release`](CTable::release),
///   and otherwise keeps the contract C states for the slot. It is callable
///   on any CPU the crate builds for: no `#[target_feature]`.
/// - Every slot [`CSlots::base`] holds that `install` does not set is NULL
///   only where C's contract lets that slot be NULL, and otherwise a function
///   C's contract lets every table of this type hold; every other field holds
///   a value C accepts. A slot C calls unconditionally is never left NULL.
pub unsafe trait CVtable<S>: CSlots<S> {}

/// The table a [`CTable<Self, S>`] starts from ([`base`](Self::base)) and
/// the slots it sets in it ([`install`](Self::install)), once, when it is
/// built. Its promises are [`CVtable`]'s, which a `CTable` also requires.
pub trait CSlots<S>: CCell {
    /// The table before `install` sets its slots, built anew for each
    /// [`CTable`].
    fn base() -> Self::C;

    /// Set the slots in `table`, a table no one else reaches yet. Called by
    /// [`CTable`] only: `Install` cannot be built outside ffibox, and is
    /// branded with the table's borrow and the impl's types, so it neither
    /// outlives the call nor serves another impl.
    fn install<'a>(table: &'a mut Self::C, _: Install<'a, Self, S>);
}

/// Proof that [`CTable`] is building the table [`CSlots::install`] fills:
/// no code outside ffibox can make one, its lifetime is that one borrow of
/// the table (invariant, so it cannot be stretched to a longer one), and its
/// types are the impl's. So it serves no other impl, and outlives no `install`
/// call: [`impl_cvtable!`](crate::impl_cvtable)'s slots are installed only
/// under it, in the table it came with. A hand-written `install` holding it
/// can still call itself on another table of its type; that is its
/// `unsafe impl CVtable`'s to exclude, as the rest of what `install` does.
///
/// ```compile_fail
/// # use core::ffi::c_int;
/// # mod ffi {
/// #     use core::ffi::c_int;
/// #     #[repr(C)]
/// #     #[derive(Default)]
/// #     pub struct ops { pub get: Option<unsafe extern "C" fn(*mut ops) -> c_int> }
/// # }
/// # ffibox::define_ctype!(Ops, OpsRef, OpsMut, ffi::ops);
/// # ffibox::impl_cvtable! {
/// #     // SAFETY: C passes `get` its table.
/// #     unsafe impl CVtable<u8> for Ops {}
/// #     base = ffi::ops::default();
/// #     fn get(@t) -> c_int { on panic: -1, on busy: -2, |_: &u8| 0 }
/// # }
/// use ffibox::vtable::{CSlots, Install};
/// let mut theirs = ffi::ops::default();
/// <Ops as CSlots<u8>>::install(&mut theirs, Install(Default::default()));
/// ```
///
/// A hand-written `install` can neither keep its token nor hand it to
/// another impl's:
///
/// ```compile_fail
/// # use core::ffi::c_int;
/// # mod ffi {
/// #     use core::ffi::c_int;
/// #     #[repr(C)]
/// #     #[derive(Default)]
/// #     pub struct ops { pub get: Option<unsafe extern "C" fn(*mut ops) -> c_int> }
/// # }
/// # ffibox::define_ctype!(Ops, OpsRef, OpsMut, ffi::ops);
/// # ffibox::define_ctype!(Other, OtherRef, OtherMut, ffi::ops);
/// # ffibox::impl_cvtable! {
/// #     // SAFETY: C passes `get` its table.
/// #     unsafe impl CVtable<u8> for Ops {}
/// #     base = ffi::ops::default();
/// #     fn get(@t) -> c_int { on panic: -1, on busy: -2, |_: &u8| 0 }
/// # }
/// use ffibox::vtable::{CSlots, Install};
/// impl CSlots<()> for Other {
///     fn base() -> ffi::ops {
///         ffi::ops::default()
///     }
///     fn install<'a>(table: &'a mut ffi::ops, token: Install<'a, Self, ()>) {
///         <Ops as CSlots<u8>>::install(table, token);
///     }
/// }
/// ```
///
/// ```compile_fail
/// # use core::ffi::c_int;
/// # mod ffi {
/// #     use core::ffi::c_int;
/// #     #[repr(C)]
/// #     #[derive(Default)]
/// #     pub struct ops { pub get: Option<unsafe extern "C" fn(*mut ops) -> c_int> }
/// # }
/// # ffibox::define_ctype!(Other, OtherRef, OtherMut, ffi::ops);
/// use std::sync::Mutex;
/// use ffibox::vtable::{CSlots, Install};
/// static KEPT: Mutex<Option<Install<'static, Other, ()>>> = Mutex::new(None);
/// impl CSlots<()> for Other {
///     fn base() -> ffi::ops {
///         ffi::ops::default()
///     }
///     fn install<'a>(_: &'a mut ffi::ops, token: Install<'a, Self, ()>) {
///         *KEPT.lock().unwrap() = Some(token);
///     }
/// }
/// ```
pub struct Install<'a, T, S>(PhantomData<Brand<'a, T, S>>);

/// `Install`'s brand: invariant in `'a`, naming `T` and `S`, and `Send + Sync`
/// whatever they are.
type Brand<'a, T, S> = (fn(&'a ()) -> &'a (), fn() -> (T, S));

impl<T, S> fmt::Debug for Install<'_, T, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Install")
    }
}

/// A layout type whose handles stay on their thread: `T::Ref` and `T::Mut`
/// are neither `Send` nor `Sync`, as [`define_ctype!`](crate::define_ctype)
/// makes them unless the binding opts in. It lets
/// [`CTable::as_ref_local`] and [`CTable::as_mut_local`] lend them over a
/// state of any thread-safety.
///
/// [`impl_clocal_handles!`](crate::impl_clocal_handles) implements it
/// checked: it fails to compile where a handle is `Send` or `Sync`. Prefer
/// it to an impl written by hand.
///
/// # Safety
///
/// `Self::Ref<'_>` and `Self::Mut<'_>` implement neither `Send` nor `Sync`,
/// and nothing derived from them that reaches the table does either: no
/// Rust object the binding builds from a handle, keeping its pointer, is
/// `Send` or `Sync`. A handle method that passes the table to a C call
/// running slots on threads of C's own is use off the thread, and that C
/// call keeps the thread rule [`CTable::into_c_local`] states.
pub unsafe trait CLocalHandles: CCell {}

/// What runs in a slot: a closure over `&S` ([`Shared`]) or `&mut S`
/// ([`Exclusive`]), told apart by the type of its argument, so a slot takes
/// the receiver its body declares. Sealed: implemented for those closures
/// only.
pub trait SlotBody<S, R, M>: sealed::Sealed<S, R, M> {}

/// The receiver of a slot whose body takes `&S`.
#[derive(Debug)]
pub enum Shared {}

/// The receiver of a slot whose body takes `&mut S`.
#[derive(Debug)]
pub enum Exclusive {}

mod sealed {
    pub trait Sealed<S, R, M> {
        /// Whether the body takes `&mut S`.
        const EXCLUSIVE: bool;

        /// Run the body on the state at `state`.
        ///
        /// # Safety
        ///
        /// `state` is live and initialised for the call; if `EXCLUSIVE`,
        /// nothing else reaches it during the call, and otherwise nothing
        /// writes it.
        unsafe fn call(self, state: *mut S) -> R;
    }
}

impl<S, R, F: FnOnce(&S) -> R> SlotBody<S, R, Shared> for F {}

impl<S, R, F: FnOnce(&S) -> R> sealed::Sealed<S, R, Shared> for F {
    const EXCLUSIVE: bool = false;

    #[inline]
    unsafe fn call(self, state: *mut S) -> R {
        // SAFETY: per this function's contract, a live state nothing writes.
        self(unsafe { &*state })
    }
}

impl<S, R, F: FnOnce(&mut S) -> R> SlotBody<S, R, Exclusive> for F {}

impl<S, R, F: FnOnce(&mut S) -> R> sealed::Sealed<S, R, Exclusive> for F {
    const EXCLUSIVE: bool = true;

    #[inline]
    unsafe fn call(self, state: *mut S) -> R {
        // SAFETY: per this function's contract, a live state nothing else
        // reaches.
        self(unsafe { &mut *state })
    }
}

// ---------------------------------------------------------------------------
// The entry state
// ---------------------------------------------------------------------------

/// An exclusive slot runs.
const EXCLUSIVE: usize = 1 << (usize::BITS - 1);
/// The table's holder, its owner or C, has released it.
const RELEASED: usize = 1 << (usize::BITS - 2);
/// A [`CTable`] owner holds the table.
const OWNED: usize = 1 << (usize::BITS - 3);
/// The owner has lent `&S` ([`CTable::state`]); cleared by its next
/// `&mut self`.
const LENT: usize = 1 << (usize::BITS - 4);
/// The number of shared slots running.
const COUNT: usize = LENT - 1;

/// Which slots run on a table, and who holds it.
struct Entries(AtomicUsize);

impl Entries {
    /// Apply `f` atomically: the previous value, or the one `f` refused. As
    /// `fetch_update`, which is deprecated for `try_update`, newer than the
    /// minimum supported Rust.
    fn update(&self, f: impl Fn(usize) -> Option<usize>) -> Result<usize, usize> {
        let mut v = self.0.load(Ordering::Acquire);
        loop {
            let next = f(v).ok_or(v)?;
            match self
                .0
                .compare_exchange_weak(v, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(prev) => return Ok(prev),
                Err(now) => v = now,
            }
        }
    }

    fn enter_shared(&self) -> bool {
        self.update(|v| {
            let open = v & (EXCLUSIVE | RELEASED) == 0 && v & COUNT < COUNT;
            open.then_some(v + 1)
        })
        .is_ok()
    }

    /// Whether the allocation is to be dropped now.
    fn leave_shared(&self) -> bool {
        let now = self.0.fetch_sub(1, Ordering::AcqRel) - 1;
        Self::settled(now)
    }

    fn enter_exclusive(&self) -> bool {
        self.update(|v| (v & !OWNED == 0).then_some(v | EXCLUSIVE))
            .is_ok()
    }

    /// Whether the allocation is to be dropped now.
    fn leave_exclusive(&self) -> bool {
        let now = self.0.fetch_and(!EXCLUSIVE, Ordering::AcqRel) & !EXCLUSIVE;
        Self::settled(now)
    }

    /// Release by C (`owner == false`) or by the owner: whether the
    /// allocation is to be dropped now.
    fn release(&self, owner: bool) -> bool {
        let clear = if owner { OWNED | LENT } else { 0 };
        let (Ok(prev) | Err(prev)) = self.update(|v| Some((v & !clear) | RELEASED));
        if !owner && prev & RELEASED != 0 {
            // A second release by C: the first one settled it.
            return false;
        }
        Self::settled((prev & !clear) | RELEASED)
    }

    /// Released, with no owner and no slot running.
    fn settled(v: usize) -> bool {
        v == RELEASED
    }

    /// Lend `&S` to the owner; refused while an exclusive slot runs.
    fn lend(&self) -> bool {
        self.update(|v| (v & EXCLUSIVE == 0).then_some(v | LENT))
            .is_ok()
    }

    /// The owner's `&mut self` proves every `&S` it lent has ended.
    fn unlend(&self) {
        self.0.fetch_and(!LENT, Ordering::AcqRel);
    }

    /// Whether a slot runs.
    fn running(&self) -> bool {
        self.0.load(Ordering::Acquire) & (COUNT | EXCLUSIVE) != 0
    }
}

/// A table's allocation: the C table first, so a pointer to it is a pointer
/// to the allocation, then the entry state, then the state.
#[repr(C)]
struct Repr<T: CCell, S> {
    table: T::C,
    entries: Entries,
    state: S,
}

impl<T: CCell, S> Repr<T, S> {
    /// The table is at offset zero: a pointer to it is one to the allocation.
    const TABLE_FIRST: () = assert!(core::mem::offset_of!(Self, table) == 0);

    /// The allocation the table at `table` heads.
    fn from_table(table: *mut T::C) -> *mut Self {
        let () = Self::TABLE_FIRST;
        table.cast()
    }

    /// [`from_table`](Self::from_table), non-null.
    fn of_table(table: NonNull<T::C>) -> NonNull<Self> {
        let () = Self::TABLE_FIRST;
        table.cast()
    }

    /// The table heading the allocation at `repr`.
    fn table(repr: NonNull<Self>) -> NonNull<T::C> {
        let () = Self::TABLE_FIRST;
        repr.cast()
    }

    /// The entry state of the live allocation at `repr`.
    ///
    /// # Safety
    ///
    /// `repr` is live for `'a`.
    unsafe fn entries<'a>(repr: *mut Self) -> &'a Entries {
        // SAFETY: per this function's contract; the entry state is Rust's,
        // never written by C, and only ever reached shared.
        unsafe { &(*repr).entries }
    }

    /// Drop the allocation.
    ///
    /// # Safety
    ///
    /// `repr` came from `Box::leak` in [`CTable::new`], and nothing reaches
    /// it again: no reference into it is live.
    unsafe fn free(repr: *mut Self) {
        // SAFETY: per this function's contract.
        drop(unsafe { Box::from_raw(repr) });
    }
}

// ---------------------------------------------------------------------------
// CTable
// ---------------------------------------------------------------------------

/// Rust state `S` behind the C table of the layout type `T`, which C calls
/// into: `Box<dyn Trait>` handed to C, the table embedded. The sole owner of
/// the allocation while Rust holds it.
///
/// [`new`](Self::new) builds the table from a base value, with the slots
/// [`CSlots::install`] sets, and the state. The owner lends the table's
/// handles ([`as_ref`](Self::as_ref) / [`as_mut`](Self::as_mut)), so a
/// binding's methods on them call the slots as on a C table, and the state
/// ([`state`](Self::state) / [`state_mut`](Self::state_mut) /
/// [`into_inner`](Self::into_inner)). [`as_c_ptr`](Self::as_c_ptr) lends
/// the table to C for a call; [`into_c`](Self::into_c) surrenders it to C,
/// which releases it through [`release`](Self::release).
///
/// C may use a lent table only while the borrow that lent it lives, and may
/// not release it; a release it makes anyway is deferred to the owner's drop.
///
/// **Threads.** `Send` and `Sync` as `RwLock<S>` is, and as `T` is: a slot
/// entered through a shared borrow on another thread reaches `&S`, or `&mut S`
/// alone. The handles are `T`'s, whose `Send` and `Sync` the binding chose for
/// the C type alone, so [`as_ref`](Self::as_ref) and [`as_mut`](Self::as_mut)
/// ask `S: Send + Sync`, and their `_local` forms ask the handles to stay on
/// their thread ([`CLocalHandles`]). [`into_c`](Self::into_c) asks
/// `S: Send + Sync` too, and [`into_c_local`](Self::into_c_local) states the
/// threads C uses instead. [`as_c_ptr`](Self::as_c_ptr) asserts nothing, as a
/// raw pointer does: the C call it is passed to keeps the same thread rule.
pub struct CTable<T: CCell, S> {
    repr: NonNull<Repr<T, S>>,
    _owns: PhantomData<Box<Repr<T, S>>>,
}

// SAFETY: the owner holds the allocation alone, as a `Box` does: moving it
// moves the table (`T: Send`) and the state (`S: Send`).
unsafe impl<T: CCell + Send, S: Send> Send for CTable<T, S> {}

// SAFETY: a shared owner reaches `T`'s shared handle (`T: Sync`), `&S`
// (`S: Sync`), and, through slots entered on a shared borrow, `&mut S` one
// entry at a time, as `RwLock<S>`'s write guard does (`S: Send`).
unsafe impl<T: CCell + Sync, S: Send + Sync> Sync for CTable<T, S> {}

impl<T: CVtable<S>, S> CTable<T, S> {
    /// The table [`CSlots::base`], its slots set by [`CSlots::install`],
    /// over `state`.
    #[must_use]
    pub fn new(state: S) -> Self {
        // SAFETY: `CVtable`'s contract makes `base` such a table.
        unsafe { Self::with_base(T::base(), state) }
    }

    /// As [`new`](Self::new), from a table the caller built: a field set per
    /// table, such as a flag, or a table a C routine initialised.
    ///
    /// # Safety
    ///
    /// As [`CSlots::base`]'s result: every slot of `base` that
    /// [`CSlots::install`] does not set is NULL, or a function C's contract
    /// lets every table of this type hold, and every other field holds a
    /// value C accepts. A slot the binding's handles call is not left to the
    /// caller's choice by any safe path.
    ///
    /// Safe code takes the table [`CSlots::base`] gives, and chooses no slot:
    ///
    /// ```compile_fail
    /// # use core::ffi::c_int;
    /// # mod ffi {
    /// #     use core::ffi::c_int;
    /// #     #[repr(C)]
    /// #     #[derive(Default)]
    /// #     pub struct store {
    /// #         pub get: Option<unsafe extern "C" fn(*mut store) -> c_int>,
    /// #         pub count: Option<unsafe extern "C" fn(*mut store) -> c_int>,
    /// #     }
    /// # }
    /// # ffibox::define_ctype!(Store, StoreRef, StoreMut, ffi::store);
    /// # ffibox::impl_cvtable! {
    /// #     // SAFETY: C passes `get` its store.
    /// #     unsafe impl CVtable<u8> for Store {}
    /// #     base = ffi::store::default();
    /// #     fn get(@t) -> c_int { on panic: -1, on busy: -2, |_: &u8| 0 }
    /// # }
    /// unsafe extern "C" fn someone_elses(_: *mut ffi::store) -> c_int { 0 }
    /// let base = ffi::store { count: Some(someone_elses), ..Default::default() };
    /// let table = ffibox::CTable::<Store, _>::with_base(base, 0u8);
    /// ```
    #[must_use]
    pub unsafe fn with_base(base: T::C, state: S) -> Self {
        let mut repr = Box::new(Repr::<T, S> {
            table: base,
            entries: Entries(AtomicUsize::new(OWNED)),
            state,
        });
        T::install(&mut repr.table, Install(PhantomData));
        Self {
            repr: NonNull::from(Box::leak(repr)),
            _owns: PhantomData,
        }
    }

    /// Adopt a table [`into_c`](Self::into_c) surrendered.
    ///
    /// # Safety
    ///
    /// `table` came from `into_c` of a `CTable<T, S>`, C has not released it
    /// and will neither use nor release it again, and no slot runs on it.
    #[must_use]
    pub unsafe fn from_c(table: NonNull<T::C>) -> Self {
        let repr = Repr::<T, S>::of_table(table);
        // SAFETY: per this function's contract, a live allocation with no
        // holder, which the owner takes.
        unsafe { Repr::entries(repr.as_ptr()) }
            .0
            .fetch_or(OWNED, Ordering::AcqRel);
        Self {
            repr,
            _owns: PhantomData,
        }
    }

    /// Run one slot's `body` on the table at `table`, with the receiver its
    /// argument's type declares; `on_busy()` if the slot is refused, and
    /// `on_panic()` if the body panics.
    ///
    /// The trampoline's whole body: [`impl_cvtable!`](crate::impl_cvtable)
    /// writes it, and a slot written by hand calls it the same way.
    ///
    /// # Safety
    ///
    /// `table` is the table of a live `CTable<T, S>` allocation: the pointer
    /// C passes back to one of its slots, or that its owner lent. It stays
    /// allocated during the call, but for a [`release`](Self::release), which
    /// the entry defers.
    #[inline]
    pub unsafe fn enter<R, M, B: SlotBody<S, R, M>>(
        table: *mut T::C,
        on_panic: impl FnOnce() -> R,
        on_busy: impl FnOnce() -> R,
        body: B,
    ) -> R {
        let repr = Repr::<T, S>::from_table(table);
        // SAFETY: per this function's contract the allocation is live for the
        // call.
        let entries = unsafe { Repr::entries(repr) };
        let entered = if B::EXCLUSIVE {
            entries.enter_exclusive()
        } else {
            entries.enter_shared()
        };
        if !entered {
            return on_busy();
        }
        // SAFETY: as above; the projection forms no reference.
        let state = unsafe { &raw mut (*repr).state };
        // SAFETY: the entry excludes, until it is left, every other slot and
        // owner borrow that would alias the receiver: all of them for `&mut S`,
        // and those reaching `&mut S` for `&S`. The table, which C may read
        // and write meanwhile, is a separate field.
        let call = || unsafe { body.call(state) };
        #[cfg(feature = "std")]
        let ret = crate::callback::barrier(|| Ok(call()), || Err(on_panic));
        #[cfg(not(feature = "std"))]
        // A panic unwinds to the `extern "C"` trampoline, which aborts.
        let ret: Result<R, core::convert::Infallible> = Ok({
            let _ = on_panic;
            call()
        });
        let settled = if B::EXCLUSIVE {
            entries.leave_exclusive()
        } else {
            entries.leave_shared()
        };
        if settled {
            // SAFETY: the last entry out of a released table with no owner:
            // nothing else reaches the allocation, and `entries` is not used
            // again.
            unsafe { Repr::free(repr) };
        }
        match ret {
            Ok(ret) => ret,
            #[cfg(feature = "std")]
            Err(on_panic) => on_panic(),
        }
    }

    /// The release slot: drops the table's allocation, state and all, once.
    ///
    /// Deferred while a slot runs on the table, until the last one exits, and
    /// while an owner holds it, until the owner drops; a slot entered while
    /// the drop is deferred is refused. Once dropped, the table is gone, and
    /// C calls nothing on it again. `S` is dropped on the thread that drops
    /// the allocation, C's releasing one or a slot's: see
    /// [`into_c`](Self::into_c) for the threads C may use. A panic in `S`'s
    /// drop aborts, as it cannot unwind into C. NULL is ignored.
    ///
    /// # Safety
    ///
    /// `table` is NULL or the table of a live `CTable<T, S>` allocation,
    /// released by its holder, which does not use it again.
    pub unsafe extern "C" fn release(table: *mut T::C) {
        if table.is_null() {
            return;
        }
        let repr = Repr::<T, S>::from_table(table);
        // SAFETY: per this function's contract the allocation is live.
        if unsafe { Repr::entries(repr) }.release(false) {
            // SAFETY: released, with no owner and no slot running: nothing
            // reaches the allocation any more.
            unsafe { Repr::free(repr) };
        }
    }
}

impl<T: CCell, S> CTable<T, S> {
    /// The table, for a C call that uses it while `self` is borrowed and
    /// keeps nothing. That call keeps [`into_c_local`](Self::into_c_local)'s
    /// thread rule for the slots it runs.
    #[inline]
    #[must_use]
    pub fn as_c_ptr(&self) -> *mut T::C {
        Repr::table(self.repr).as_ptr()
    }

    /// Surrender the table to C, which releases it through
    /// [`release`](Self::release).
    ///
    /// C keeps it past any borrow, so `S` borrows nothing (`S: 'static`), and
    /// may call it and release it on any thread, so `S: Send + Sync`, as
    /// [`CPayload::into_raw`](crate::CPayload::into_raw) asks: a shared slot
    /// shares `&S`, and may drop `S` there as the last one out of a deferred
    /// release. For a C library that keeps to `S`'s threads,
    /// [`into_c_local`](Self::into_c_local). A release C made of the table
    /// while it was only lent is forgotten: C now holds it.
    ///
    /// A state that borrows stays with Rust:
    ///
    /// ```compile_fail
    /// # use core::ffi::c_int;
    /// # mod ffi {
    /// #     use core::ffi::c_int;
    /// #     #[repr(C)]
    /// #     #[derive(Default)]
    /// #     pub struct counter { pub get: Option<unsafe extern "C" fn(*mut counter) -> c_int> }
    /// # }
    /// # ffibox::define_ctype!(Counter, CounterRef, CounterMut, ffi::counter);
    /// impl<'a> ffibox::vtable::CSlots<&'a c_int> for Counter {
    ///     fn base() -> ffi::counter {
    ///         ffi::counter::default()
    ///     }
    ///     fn install<'t>(_: &'t mut ffi::counter, _: ffibox::vtable::Install<'t, Self, &'a c_int>) {}
    /// }
    /// // SAFETY: installs nothing.
    /// unsafe impl<'a> ffibox::vtable::CVtable<&'a c_int> for Counter {}
    /// fn surrender(n: &c_int) -> *mut ffi::counter {
    ///     ffibox::CTable::<Counter, _>::new(n).into_c().as_ptr()
    /// }
    /// ```
    ///
    /// Nor does one that is not `Send` and `Sync` go to C's threads:
    ///
    /// ```compile_fail
    /// # use core::ffi::c_int;
    /// # use std::rc::Rc;
    /// # mod ffi {
    /// #     use core::ffi::c_int;
    /// #     #[repr(C)]
    /// #     #[derive(Default)]
    /// #     pub struct counter { pub get: Option<unsafe extern "C" fn(*mut counter) -> c_int> }
    /// # }
    /// # ffibox::define_ctype!(Counter, CounterRef, CounterMut, ffi::counter);
    /// ffibox::impl_cvtable! {
    ///     // SAFETY: C passes `get` its counter.
    ///     unsafe impl CVtable<Rc<c_int>> for Counter {}
    ///     base = ffi::counter::default();
    ///     fn get(@t) -> c_int { on panic: -1, on busy: -2, |c: &Rc<c_int>| **c }
    /// }
    /// let p = ffibox::CTable::<Counter, _>::new(Rc::new(1)).into_c();
    /// ```
    #[must_use = "the returned pointer owns the table and must be released"]
    pub fn into_c(self) -> NonNull<T::C>
    where
        S: Send + Sync + 'static,
    {
        // SAFETY: `S: Send + Sync` keeps every thread C may use within `S`'s
        // terms.
        unsafe { self.into_c_local() }
    }

    /// As [`into_c`](Self::into_c), for a state that need not be `Send` or
    /// `Sync`.
    ///
    /// # Safety
    ///
    /// C calls the table's slots, and releases it, on threads `S` allows: on
    /// any other than the one `S` belongs to only if `S: Send + Sync` for
    /// shared slots, and `S: Send` for exclusive slots and the release.
    #[must_use = "the returned pointer owns the table and must be released"]
    pub unsafe fn into_c_local(self) -> NonNull<T::C>
    where
        S: 'static,
    {
        let this = ManuallyDrop::new(self);
        this.entries()
            .0
            .fetch_and(!(OWNED | LENT | RELEASED), Ordering::AcqRel);
        Repr::table(this.repr)
    }

    /// The table's shared handle.
    ///
    /// The binding's methods on it may call any slot, and the handle may
    /// cross threads where the binding made `T::Ref` `Send` or `Sync`, so `S`
    /// is `Send + Sync`, as for a `&RwLock<S>` shared between threads. Where
    /// the handles stay on their thread ([`CLocalHandles`]),
    /// [`as_ref_local`](Self::as_ref_local) takes any `S`.
    ///
    /// A state that is not `Sync` is not reached through a handle the
    /// binding made `Send`:
    ///
    /// ```compile_fail
    /// # use core::cell::Cell;
    /// # use core::ffi::c_int;
    /// # mod ffi {
    /// #     use core::ffi::c_int;
    /// #     #[repr(C)]
    /// #     #[derive(Default)]
    /// #     pub struct counter { pub get: Option<unsafe extern "C" fn(*mut counter) -> c_int> }
    /// # }
    /// ffibox::define_ctype!(Counter, CounterRef, CounterMut, ffi::counter);
    /// // SAFETY: the C library's counters are thread-safe.
    /// unsafe impl Send for CounterRef<'_> {}
    /// ffibox::impl_cvtable! {
    ///     // SAFETY: C passes `get` its counter.
    ///     unsafe impl CVtable<Cell<c_int>> for Counter {}
    ///     base = ffi::counter::default();
    ///     fn get(@t) -> c_int { on panic: -1, on busy: -2, |c: &Cell<c_int>| c.get() }
    /// }
    /// let table = ffibox::CTable::<Counter, _>::new(Cell::new(0));
    /// let r = table.as_ref(); // `Cell<c_int>: !Sync`
    /// std::thread::scope(|s| drop(s.spawn(move || r)));
    /// ```
    #[must_use]
    pub fn as_ref(&self) -> T::Ref<'_>
    where
        S: Send + Sync,
    {
        self.handle_ref()
    }

    /// As [`as_ref`](Self::as_ref), for any `S`, where the handles stay on
    /// their thread. A handle method passing the table to a C call that runs
    /// slots on threads of C's own is use off the thread: that C call keeps
    /// the thread rule [`into_c_local`](Self::into_c_local) states.
    ///
    /// A layout type that is not [`CLocalHandles`] has no local form:
    ///
    /// ```compile_fail
    /// # use core::cell::Cell;
    /// # use core::ffi::c_int;
    /// # mod ffi {
    /// #     use core::ffi::c_int;
    /// #     #[repr(C)]
    /// #     #[derive(Default)]
    /// #     pub struct counter { pub get: Option<unsafe extern "C" fn(*mut counter) -> c_int> }
    /// # }
    /// ffibox::define_ctype!(Counter, CounterRef, CounterMut, ffi::counter);
    /// // SAFETY: the C library's counters are thread-safe.
    /// unsafe impl Send for CounterRef<'_> {}
    /// ffibox::impl_cvtable! {
    ///     // SAFETY: C passes `get` its counter.
    ///     unsafe impl CVtable<Cell<c_int>> for Counter {}
    ///     base = ffi::counter::default();
    ///     fn get(@t) -> c_int { on panic: -1, on busy: -2, |c: &Cell<c_int>| c.get() }
    /// }
    /// let table = ffibox::CTable::<Counter, _>::new(Cell::new(0));
    /// let r = table.as_ref_local();
    /// ```
    #[must_use]
    pub fn as_ref_local(&self) -> T::Ref<'_>
    where
        T: CLocalHandles,
    {
        self.handle_ref()
    }

    /// The table's exclusive handle.
    ///
    /// The binding's methods on it may call any slot, the handle may move to
    /// another thread where the binding made `T::Mut` `Send`, and the shared
    /// handles it lends spread over threads where `T::Ref` is `Send` or
    /// `Sync`, so `S` is `Send + Sync`, as for [`as_ref`](Self::as_ref).
    /// Where the handles stay on their thread ([`CLocalHandles`]),
    /// [`as_mut_local`](Self::as_mut_local) takes any `S`.
    #[must_use]
    pub fn as_mut(&mut self) -> T::Mut<'_>
    where
        S: Send + Sync,
    {
        self.handle_mut()
    }

    /// As [`as_mut`](Self::as_mut), for any `S`, where the handles stay on
    /// their thread; the C calls its methods make keep the thread rule,
    /// as for [`as_ref_local`](Self::as_ref_local).
    #[must_use]
    pub fn as_mut_local(&mut self) -> T::Mut<'_>
    where
        T: CLocalHandles,
    {
        self.handle_mut()
    }

    // The handles, for `as_ref` / `as_mut` and their `_local` forms, whose
    // bounds keep every thread a handle reaches within `S`'s terms.
    fn handle_ref(&self) -> T::Ref<'_> {
        // SAFETY: the live table this owner holds, initialised by `new`;
        // Rust writes it only through `as_mut`, which `&self` excludes, and C
        // only while lent it through a handle.
        unsafe { T::ref_from_ptr(self.as_c_ptr()) }
            .unwrap_or_else(|| unreachable!("a box is never NULL"))
    }

    fn handle_mut(&mut self) -> T::Mut<'_> {
        self.entries().unlend();
        // SAFETY: as in `handle_ref`, exclusive for `&mut self`; the pointer
        // carries the allocation's write provenance.
        unsafe { T::mut_from_ptr(self.as_c_ptr()) }
            .unwrap_or_else(|| unreachable!("a box is never NULL"))
    }

    /// The state, shared; `None` while an exclusive slot runs.
    ///
    /// Exclusive slots are refused from here until the owner is next borrowed
    /// mutably, which proves the `&S` has ended.
    #[must_use]
    pub fn state(&self) -> Option<&S> {
        let repr = self.repr.as_ptr();
        // SAFETY: once lent, exclusive slots are refused for as long as the
        // `&S` can live, `&self`'s borrow, and none runs now.
        self.entries().lend().then(|| unsafe { &(*repr).state })
    }

    /// The state, exclusively.
    pub fn state_mut(&mut self) -> &mut S {
        self.entries().unlend();
        // SAFETY: `&mut self` excludes the owner's other borrows, and so every
        // slot: C reaches a lent table only while the borrow lending it lives.
        unsafe { &mut (*self.repr.as_ptr()).state }
    }

    /// The state, the table dropped.
    ///
    /// # Panics
    ///
    /// If a slot runs, which only C using a table it was not lent can make
    /// happen.
    #[must_use]
    pub fn into_inner(self) -> S {
        assert!(
            !self.entries().running(),
            "ffibox: a CTable's state taken while one of its slots runs"
        );
        let this = ManuallyDrop::new(self);
        // SAFETY: the allocation from `Box::leak` in `new`, which this owner
        // holds and no slot reaches; `this` is never dropped.
        let repr = unsafe { Box::from_raw(this.repr.as_ptr()) };
        repr.state
    }

    fn entries(&self) -> &Entries {
        // SAFETY: the allocation this owner holds is live while `&self` is.
        unsafe { Repr::entries(self.repr.as_ptr()) }
    }
}

impl<T: CCell, S> Drop for CTable<T, S> {
    fn drop(&mut self) {
        let repr = self.repr.as_ptr();
        if self.entries().release(true) {
            // SAFETY: released, the owner gone, no slot running.
            unsafe { Repr::free(repr) };
        }
    }
}

impl<T: CCell, S> fmt::Debug for CTable<T, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CTable").finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// impl_cvtable!
// ---------------------------------------------------------------------------

/// Implement a C table's slots in Rust: the [`CVtable<S>`](crate::vtable::CVtable)
/// and [`CSlots<S>`](crate::vtable::CSlots) impls for its layout type `T`,
/// which build the table from its `base` and install one `extern "C"`
/// trampoline per slot and, if C frees the table,
/// [`CTable::release`](crate::vtable::CTable::release).
///
/// ```ignore
/// ffibox::impl_cvtable! {
///     // SAFETY: libgit2 passes a stream's members the stream, first, and
///     // calls `write`, `close` and `free` on every stream.
///     unsafe impl<W: StreamWriter> CVtable<W> for WriteStream {}
///     base = ffi::git_writestream::default();
///     /// `git_writestream.write` over `StreamWriter::write`.
///     fn write(@stream, buffer: *const c_char, len: usize) -> c_int {
///         on panic: std::process::abort(),
///         on busy: GIT_EUSER,
///         |w: &mut W| returned(w.write(unsafe { bytes(buffer, len) })),
///     }
///     fn close(@stream) -> c_int { /* … */ }
///     release free;
/// }
/// ```
///
/// - `unsafe impl<…> CVtable<S> for T {}`: the state `S` and the layout type
///   `T` ([`define_ctype!`](crate::define_ctype)'s, or any `CCell`, named by
///   a path with type arguments, if any); each generic takes one trait-path
///   bound. The line is the binding's `unsafe impl` of
///   [`CVtable`](crate::vtable::CVtable), emitted with the binding's own
///   `unsafe` and braces: `#![forbid(unsafe_code)]` rejects it, clippy's
///   `undocumented_unsafe_blocks` asks for its `SAFETY` comment, and that
///   comment states `CVtable`'s contract: where C passes each slot its
///   table, and that no slot C calls is left NULL.
/// - `base = expr;`: the table before the slots are set. Written out, never
///   defaulted: `Default::default()`, all slots NULL for a bindgen struct, is
///   right only when every slot C calls unconditionally is declared below.
/// - `fn slot(…)`: the C field, then its C arguments in C's order, the one C
///   passes the table in marked `@name`, which names the table pointer in the
///   body. Only doc comments go before it: the trampoline takes no other
///   attribute. The trampoline has exactly this signature, so a slot that
///   does not match the C field's type does not compile, nor, in most
///   tables, does one marking the wrong argument; but where another argument
///   has the table's type too (`cmp(ops *self, ops *other)`), the types
///   cannot tell them apart, and the `@` marks the one C's documentation
///   names, as the `SAFETY` comment states.
/// - `on panic:` and `on busy:`: what the slot returns when its body panics,
///   or when it is refused (see [`CTable::enter`](crate::vtable::CTable::enter));
///   expressions, evaluated only then.
/// - the body: a closure over the receiver, `|s: &S|` for a shared slot or
///   `|s: &mut S|` for an exclusive one; the C arguments are captured by name.
/// - `release field;`: the slot C frees the table with, if it does.
///
/// The trampolines are local to `install`, which takes a token only
/// [`CTable`](crate::vtable::CTable) makes: no other code can name a
/// trampoline or install one, so each reaches the state only through the
/// table it was installed in.
///
/// A body whose receiver is not `&S` or `&mut S`, or whose result is not the
/// slot's, does not compile:
///
/// ```compile_fail
/// # use core::ffi::c_int;
/// # mod ffi {
/// #     use core::ffi::c_int;
/// #     #[repr(C)]
/// #     #[derive(Default)]
/// #     pub struct ops {
/// #         pub get: Option<unsafe extern "C" fn(*mut ops) -> c_int>,
/// #         pub next: Option<unsafe extern "C" fn(*mut c_int, *mut ops) -> c_int>,
/// #         pub cmp: Option<unsafe extern "C" fn(*mut ops, *mut ops) -> c_int>,
/// #     }
/// # }
/// # ffibox::define_ctype!(Ops, OpsRef, OpsMut, ffi::ops);
/// struct Other;
/// ffibox::impl_cvtable! {
///     // SAFETY: C passes `get` its table.
///     unsafe impl CVtable<u32> for Ops {}
///     base = ffi::ops::default();
///     fn get(@ops) -> c_int {
///         on panic: -1,
///         on busy: -2,
///         |_: &Other| 0, // the state is `u32`
///     }
/// }
/// ```
///
/// Where the table's type is the only pointer of its type, marking the wrong
/// argument does not compile:
///
/// ```compile_fail
/// # use core::ffi::c_int;
/// # mod ffi {
/// #     use core::ffi::c_int;
/// #     #[repr(C)]
/// #     #[derive(Default)]
/// #     pub struct ops {
/// #         pub get: Option<unsafe extern "C" fn(*mut ops) -> c_int>,
/// #         pub next: Option<unsafe extern "C" fn(*mut c_int, *mut ops) -> c_int>,
/// #         pub cmp: Option<unsafe extern "C" fn(*mut ops, *mut ops) -> c_int>,
/// #     }
/// # }
/// # ffibox::define_ctype!(Ops, OpsRef, OpsMut, ffi::ops);
/// ffibox::impl_cvtable! {
///     // SAFETY: C passes `next` its table last.
///     unsafe impl CVtable<u32> for Ops {}
///     base = ffi::ops::default();
///     // C passes the table last: `next(int *out, ops *table)`.
///     fn next(@ops, out: *mut c_int) -> c_int {
///         on panic: -1,
///         on busy: -2,
///         |_: &u32| 0,
///     }
/// }
/// ```
///
/// The impl is `unsafe`, since types cannot check every table argument:
///
/// ```compile_fail
/// # use core::ffi::c_int;
/// # mod ffi {
/// #     use core::ffi::c_int;
/// #     #[repr(C)]
/// #     #[derive(Default)]
/// #     pub struct ops {
/// #         pub get: Option<unsafe extern "C" fn(*mut ops) -> c_int>,
/// #         pub next: Option<unsafe extern "C" fn(*mut c_int, *mut ops) -> c_int>,
/// #         pub cmp: Option<unsafe extern "C" fn(*mut ops, *mut ops) -> c_int>,
/// #     }
/// # }
/// # ffibox::define_ctype!(Ops, OpsRef, OpsMut, ffi::ops);
/// ffibox::impl_cvtable! {
///     impl CVtable<u32> for Ops {}
///     base = ffi::ops::default();
///     fn cmp(_other: *mut ffi::ops, @this) -> c_int {
///         on panic: -1,
///         on busy: -2,
///         |_: &u32| 0,
///     }
/// }
/// ```
///
/// and its `unsafe` is the binding's own, which `forbid(unsafe_code)` sees:
///
/// ```compile_fail
/// # use core::ffi::c_int;
/// # mod ffi {
/// #     use core::ffi::c_int;
/// #     #[repr(C)]
/// #     #[derive(Default)]
/// #     pub struct ops {
/// #         pub get: Option<unsafe extern "C" fn(*mut ops) -> c_int>,
/// #         pub next: Option<unsafe extern "C" fn(*mut c_int, *mut ops) -> c_int>,
/// #         pub cmp: Option<unsafe extern "C" fn(*mut ops, *mut ops) -> c_int>,
/// #     }
/// # }
/// #[forbid(unsafe_code)]
/// mod binding {
///     use core::ffi::c_int;
///     use super::ffi;
///     ffibox::define_ctype!(Ops, OpsRef, OpsMut, ffi::ops);
///     ffibox::impl_cvtable! {
///         // SAFETY: C passes `cmp` its table first.
///         unsafe impl CVtable<u32> for Ops {}
///         base = ffi::ops::default();
///         fn cmp(@this, _other: *mut ffi::ops) -> c_int {
///             on panic: -1,
///             on busy: -2,
///             |_: &u32| 0,
///         }
///     }
/// }
/// ```
///
/// The `base` is written out:
///
/// ```compile_fail
/// # use core::ffi::c_int;
/// # mod ffi {
/// #     use core::ffi::c_int;
/// #     #[repr(C)]
/// #     #[derive(Default)]
/// #     pub struct ops {
/// #         pub get: Option<unsafe extern "C" fn(*mut ops) -> c_int>,
/// #         pub next: Option<unsafe extern "C" fn(*mut c_int, *mut ops) -> c_int>,
/// #         pub cmp: Option<unsafe extern "C" fn(*mut ops, *mut ops) -> c_int>,
/// #     }
/// # }
/// # ffibox::define_ctype!(Ops, OpsRef, OpsMut, ffi::ops);
/// ffibox::impl_cvtable! {
///     // SAFETY: C passes `get` its table, and calls no other slot.
///     unsafe impl CVtable<u32> for Ops {}
///     fn get(@t) -> c_int { on panic: -1, on busy: -2, |s: &u32| *s as c_int }
/// }
/// ```
///
/// The macro's internal arms mint a trampoline only inside the `install`
/// they expand in, under the `Install` token it was handed: called directly,
/// without one, they do not compile,
///
/// ```compile_fail
/// # use core::ffi::c_int;
/// # mod ffi {
/// #     use core::ffi::c_int;
/// #     #[repr(C)]
/// #     #[derive(Default)]
/// #     pub struct ops { pub get: Option<unsafe extern "C" fn(*mut ops) -> c_int> }
/// # }
/// # ffibox::define_ctype!(Ops, OpsRef, OpsMut, ffi::ops);
/// # ffibox::impl_cvtable! {
/// #     // SAFETY: C passes `get` its table, its only argument.
/// #     unsafe impl CVtable<u64> for Ops {}
/// #     base = ffi::ops::default();
/// #     fn get(@t) -> core::ffi::c_int { on panic: -1, on busy: -2, |s: &u64| *s as core::ffi::c_int }
/// # }
/// #[forbid(unsafe_code)]
/// mod binding {
///     use core::ffi::c_int;
///     pub fn table() -> super::ffi::ops {
///         let mut table = super::ffi::ops::default();
///         ffibox::impl_cvtable!(@slot table [] super::Ops, u64;
///             fn get(@t) -> c_int { on panic: -1, on busy: -2, |s: &u64| *s as c_int }
///         );
///         table
///     }
/// }
/// ```
///
/// nor with anything else in the token's place:
///
/// ```compile_fail
/// # use core::ffi::c_int;
/// # mod ffi {
/// #     use core::ffi::c_int;
/// #     #[repr(C)]
/// #     #[derive(Default)]
/// #     pub struct ops { pub get: Option<unsafe extern "C" fn(*mut ops) -> c_int> }
/// # }
/// # ffibox::define_ctype!(Ops, OpsRef, OpsMut, ffi::ops);
/// # ffibox::impl_cvtable! {
/// #     // SAFETY: C passes `get` its table, its only argument.
/// #     unsafe impl CVtable<u64> for Ops {}
/// #     base = ffi::ops::default();
/// #     fn get(@t) -> core::ffi::c_int { on panic: -1, on busy: -2, |s: &u64| *s as core::ffi::c_int }
/// # }
/// #[forbid(unsafe_code)]
/// mod binding {
///     use core::ffi::c_int;
///     pub fn table() -> super::ffi::ops {
///         let mut ops = super::ffi::ops::default();
///         let table = &mut ops;
///         let token = ();
///         ffibox::impl_cvtable!(@slots table token [] super::Ops, u64;
///             fn get(@t) -> c_int { on panic: -1, on busy: -2, |s: &u64| *s as c_int }
///         );
///         ops
///     }
/// }
/// ```
///
/// A slot takes doc comments only: an attribute such as `#[target_feature]`
/// would make calling the trampoline `unsafe` in a way C cannot see.
///
/// ```compile_fail
/// # use core::ffi::c_int;
/// # mod ffi {
/// #     use core::ffi::c_int;
/// #     #[repr(C)]
/// #     #[derive(Default)]
/// #     pub struct ops {
/// #         pub get: Option<unsafe extern "C" fn(*mut ops) -> c_int>,
/// #         pub next: Option<unsafe extern "C" fn(*mut c_int, *mut ops) -> c_int>,
/// #         pub cmp: Option<unsafe extern "C" fn(*mut ops, *mut ops) -> c_int>,
/// #     }
/// # }
/// # ffibox::define_ctype!(Ops, OpsRef, OpsMut, ffi::ops);
/// ffibox::impl_cvtable! {
///     // SAFETY: C passes `get` its table.
///     unsafe impl CVtable<u64> for Ops {}
///     base = ffi::ops::default();
///     #[target_feature(enable = "avx2")]
///     fn get(@t) -> c_int {
///         on panic: -1,
///         on busy: -2,
///         |s: &u64| *s as c_int,
///     }
/// }
/// ```
#[macro_export]
macro_rules! impl_cvtable {
    // One slot: its trampoline, installed in `$table`. `$token` is the
    // `Install` the `install` this expands in was handed, which only a
    // `CTable` building a `$t` over `$s` makes: without one, nothing compiles.
    (@slot $table:ident $token:ident [$($g:ident $(: $b:path)?),*] $t:ty, $s:ty;
        $(#[doc = $doc:literal])*
        fn $slot:ident (
            $($pre:ident : $pty:ty ,)*
            @ $this:ident
            $(, $post:ident : $postty:ty)* $(,)?
        ) $(-> $ret:ty)? {
            on panic: $panic:expr,
            on busy: $busy:expr,
            $body:expr $(,)?
        }
    ) => {{
        let _: &$crate::vtable::Install<'_, $t, $s> = &$token;
        let _: &&mut <$t as $crate::CCell>::C = &$table;
        $(#[doc = $doc])*
        unsafe extern "C" fn __ffibox_slot<$($g $(: $b)?),*>(
            $($pre: $pty,)*
            $this: *mut <$t as $crate::CCell>::C
            $(, $post: $postty)*
        ) $(-> $ret)? {
            // Outside the `unsafe` block, so that the binding's code is
            // checked as code of its own.
            let on_panic = || $panic;
            let on_busy = || $busy;
            let body = $body;
            // SAFETY: C calls a slot with the table it read the slot from,
            // live, as the call handing it the table states, in the argument
            // the impl's `unsafe` vouches for; `install` put this slot in a
            // `CTable<$t, $s>` only.
            unsafe { $crate::vtable::CTable::<$t, $s>::enter($this, on_panic, on_busy, body) }
        }
        $table.$slot = ::core::option::Option::Some(__ffibox_slot::<$($g),*>);
    }};
    // The slots, each through `@slot`, under `@slot`'s token.
    (@slots $table:ident $token:ident $gen:tt $t:ty, $s:ty;
        $(
            $(#[doc = $doc:literal])*
            fn $slot:ident $params:tt $(-> $ret:ty)? { $($body:tt)* }
        )*
        $(release $free:ident;)?
    ) => {
        let _: &$crate::vtable::Install<'_, $t, $s> = &$token;
        let _: &&mut <$t as $crate::CCell>::C = &$table;
        $(
            $crate::impl_cvtable!(@slot $table $token $gen $t, $s;
                $(#[doc = $doc])* fn $slot $params $(-> $ret)? { $($body)* }
            );
        )*
        $(
            $table.$free =
                ::core::option::Option::Some($crate::vtable::CTable::<$t, $s>::release);
        )?
    };
    // The impl's `unsafe` is the caller's token, or nothing compiles.
    (@unsafe unsafe) => {};
    (
        $u:tt impl $(<$($g:ident $(: $b:path)?),+ $(,)?>)?
            CVtable<$s:ty> for $($tp:ident)::+ $(<$($ta:ty),+ $(,)?>)? $marker:tt
        base = $base:expr;
        $($items:tt)*
    ) => {
        $crate::impl_cvtable!(@unsafe $u);

        // The caller's own `unsafe` and braces, first and last, so the
        // caller's lints see the impl as the caller's: `unsafe_code`, and
        // clippy's `undocumented_unsafe_blocks` asking for its `SAFETY`.
        $u impl $(<$($g $(: $b)?),+>)? $crate::vtable::CVtable<$s>
            for $($tp)::+ $(<$($ta),+>)? $marker

        impl $(<$($g $(: $b)?),+>)? $crate::vtable::CSlots<$s> for $($tp)::+ $(<$($ta),+>)? {
            fn base() -> <Self as $crate::CCell>::C {
                $base
            }

            fn install<'table>(
                table: &'table mut <Self as $crate::CCell>::C,
                token: $crate::vtable::Install<'table, Self, $s>,
            ) {
                $crate::impl_cvtable!(@slots table token [$($($g $(: $b)?),+)?]
                    $($tp)::+ $(<$($ta),+>)?, $s;
                    $($items)*
                );
            }
        }
    };
}

/// Implement [`CLocalHandles`](crate::vtable::CLocalHandles) for a layout type
/// whose handles are checked, at compile time, to be neither `Send` nor
/// `Sync`: the invocation fails to compile (a type annotation error naming
/// the handle) where the binding opted one of them in.
///
/// ```ignore
/// ffibox::define_ctype!(WriteStream, WriteStreamRef, WriteStreamMut, ffi::git_writestream);
/// ffibox::impl_clocal_handles!(WriteStream);
/// ```
///
/// The check covers `T::Ref` and `T::Mut`. Objects the binding derives from
/// a handle that keep its pointer are its own to keep `!Send` and `!Sync`,
/// as [`CLocalHandles`](crate::vtable::CLocalHandles)' contract states. The
/// type is a concrete one, with no generic parameters.
///
/// ```compile_fail
/// # mod ffi {
/// #     #[repr(C)]
/// #     #[derive(Default)]
/// #     pub struct ops { pub get: Option<unsafe extern "C" fn(*mut ops) -> core::ffi::c_int> }
/// # }
/// ffibox::define_ctype!(Ops, OpsRef, OpsMut, ffi::ops);
/// // SAFETY: the C library's tables are thread-safe.
/// unsafe impl Send for OpsRef<'_> {}
/// ffibox::impl_clocal_handles!(Ops); // `OpsRef` is `Send`
/// ```
#[macro_export]
macro_rules! impl_clocal_handles {
    ($t:ty $(,)?) => {
        // Ambiguous, so an error, exactly where the handle is `Send` (or
        // `Sync`): the second impl then applies beside the first.
        const _: fn() = || {
            trait AmbiguousIfSend<A> {
                fn check() {}
            }
            impl<T: ?Sized> AmbiguousIfSend<()> for T {}
            impl<T: ?Sized + ::core::marker::Send> AmbiguousIfSend<u8> for T {}
            trait AmbiguousIfSync<A> {
                fn check() {}
            }
            impl<T: ?Sized> AmbiguousIfSync<()> for T {}
            impl<T: ?Sized + ::core::marker::Sync> AmbiguousIfSync<u8> for T {}
            let _ = <<$t as $crate::CCell>::Ref<'static> as AmbiguousIfSend<_>>::check;
            let _ = <<$t as $crate::CCell>::Ref<'static> as AmbiguousIfSync<_>>::check;
            let _ = <<$t as $crate::CCell>::Mut<'static> as AmbiguousIfSend<_>>::check;
            let _ = <<$t as $crate::CCell>::Mut<'static> as AmbiguousIfSync<_>>::check;
        };
        // SAFETY: the check above: neither handle is `Send` or `Sync`.
        unsafe impl $crate::vtable::CLocalHandles for $t {}
    };
}
