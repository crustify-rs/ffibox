//! [`CShared<T>`]: `Arc<T>` for an object Rust allocates whose count is a
//! field of the object itself, which C takes and drops too.
//!
//! A ported library often keeps C's intrusive refcount: the object starts
//! with a count field, C's macros increment and decrement it in place
//! (`GIT_REFCOUNT_INC(obj)`, `foo_free(obj)`), and the object is freed by
//! whoever drops the last reference, Rust or C. [`CArc`](crate::CArc) holds a
//! reference to such an object when C allocates it and Rust reaches it only
//! through handles. `CShared` is the outbound case: Rust allocates the
//! object, its type is a Rust struct with a C-visible count, and Rust reaches
//! it as `&T`, as `Arc<T>` does.
//!
//! The binding says how the count works once, in an `unsafe impl` of
//! [`CCounted`]: how the count is set when the object is adopted, how a
//! reference is taken and dropped, when the object is released, and how it is
//! freed. [`CShareable`] adds that a `&T` may be held while C holds
//! references too, which is what lets `CShared` deref.
//!
//! ```
//! use core::ptr::NonNull;
//! use core::sync::atomic::{AtomicU32, Ordering};
//! use ffibox::counted::{CCounted, CShareable, CShared};
//!
//! mod count {
//!     use core::sync::atomic::{AtomicI32, Ordering, fence};
//!
//!     /// `int rc`, which C increments and decrements in place with
//!     /// sequentially consistent atomics: the atomic alone, with no padding
//!     /// or other field, so a `&Count` covers nothing else. Safe code can make one only at one
//!     /// reference, and change it not at all: its field is private to this
//!     /// module and its mutators are `unsafe`.
//!     pub struct Count(AtomicI32);
//!
//!     impl Count {
//!         /// One reference: the only count safe code can make.
//!         pub const fn one() -> Self {
//!             Count(AtomicI32::new(1))
//!         }
//!         /// Back to one reference (`c_init`).
//!         /// # Safety
//!         /// Nothing else reaches the object.
//!         pub unsafe fn reset(&self) {
//!             self.0.store(1, Ordering::Relaxed);
//!         }
//!         /// Take a reference, refusing long before the count could wrap,
//!         /// as `Arc`'s bound does.
//!         /// # Safety
//!         /// The caller holds a reference.
//!         pub unsafe fn up(&self) -> bool {
//!             let mut n = self.0.load(Ordering::Relaxed);
//!             while n < i32::MAX / 2 {
//!                 match self.0.compare_exchange_weak(n, n + 1, Ordering::Relaxed, Ordering::Relaxed) {
//!                     Ok(_) => return true,
//!                     Err(now) => n = now,
//!                 }
//!             }
//!             false
//!         }
//!         /// Drop a reference: `Release`, then `Acquire` before "last".
//!         /// # Safety
//!         /// The caller gives up a reference it holds.
//!         pub unsafe fn down(&self) -> bool {
//!             if self.0.fetch_sub(1, Ordering::Release) != 1 {
//!                 return false;
//!             }
//!             fence(Ordering::Acquire);
//!             true
//!         }
//!         /// Whether one reference is left, read with `Acquire`.
//!         pub fn is_one(&self) -> bool {
//!             self.0.load(Ordering::Acquire) == 1
//!         }
//!     }
//! }
//! use count::Count;
//!
//! /// `struct session { int rc; ... }`: C calls `session_free`, below, to
//! /// drop a reference.
//! #[repr(C)]
//! pub struct Session {
//!     rc: Count,
//!     /// Rust-private state after the C-visible prefix.
//!     hits: AtomicU32,
//! }
//!
//! // SAFETY: `rc` counts the references, as C's own increment and decrement
//! // do; safe code can make a `Count` only at one reference and cannot change
//! // one (see `count`), so a `Session` stored through `get_mut` holds one.
//! // `up` refuses before the count could wrap. A decrement is `Release`, and
//! // the last one is followed by an `Acquire` fence, as `Arc`'s are, so every
//! // use through another reference happens-before the free; C's decrement
//! // is `SEQ_CST`. `c_is_sole_owner` reads with `Acquire`. Sessions live in a
//! // `Box` of `Global`, which the provided `c_release` frees.
//! unsafe impl CCounted for Session {
//!     type Alloc = std::alloc::Global;
//!     unsafe fn c_init(this: NonNull<Self>) {
//!         // SAFETY: the caller has the object to itself.
//!         unsafe { this.as_ref().rc.reset() }
//!     }
//!     unsafe fn c_up_ref(this: NonNull<Self>) -> bool {
//!         // SAFETY: a live session, on which the caller holds a reference.
//!         unsafe { this.as_ref().rc.up() }
//!     }
//!     unsafe fn c_down_ref(this: NonNull<Self>) -> bool {
//!         // Only the count is borrowed across the decrement: `Count` is the
//!         // atomic alone, so no reference covers the rest of the session
//!         // when another thread may take it (`CCounted`'s contract).
//!         // SAFETY: as above; the caller gives it up.
//!         unsafe { (*this.as_ptr()).rc.down() }
//!     }
//!     unsafe fn c_is_sole_owner(this: NonNull<Self>) -> bool {
//!         // SAFETY: as above.
//!         unsafe { this.as_ref() }.rc.is_one()
//!     }
//! }
//! // SAFETY: what C writes, and what holders write while it is shared, is
//! // atomic.
//! unsafe impl CShareable for Session {}
//!
//! impl Session {
//!     pub fn new() -> CShared<Session> {
//!         // `from_box` sets the count (`c_init`).
//!         CShared::from(Box::new(Session { rc: Count::one(), hits: AtomicU32::new(0) }))
//!     }
//!     pub fn hit(&self) -> u32 {
//!         self.hits.fetch_add(1, Ordering::Relaxed) + 1
//!     }
//! }
//!
//! /// `void session_free(session *)`, exported to C.
//! pub unsafe extern "C" fn session_free(s: *mut Session) {
//!     // A real export runs this in its panic barrier.
//!     // SAFETY: C passes NULL or a session whose reference it gives up.
//!     unsafe { CShared::release(s) }
//! }
//!
//! let a = Session::new();
//! let b = a.clone();                        // c_up_ref
//! assert_eq!(a.hit(), 1);
//! assert_eq!(b.hit(), 2);
//! // C takes a reference of its own, and drops it later.
//! let c_ref = CShared::into_raw(b.clone());
//! drop(b);
//! // SAFETY: the reference `c_ref` holds, given up once.
//! unsafe { session_free(c_ref) };
//! assert_eq!(a.hit(), 3);
//! // The last reference frees the session.
//! ```
//!
//! **The owner field.** A count that also records an owner, as libgit2's
//! `git_refcount { refcount; owner; }` does, keeps that logic in
//! [`c_down_ref`](CCounted::c_down_ref): it answers `false` while an owner
//! holds the object, and the owner releases the object when it lets go.
//!
//! **Methods are associated functions**, as `Arc`'s are: `CShared::as_ptr(&s)`,
//! `CShared::get_mut(&mut s)`, `CShared::into_raw(s)`, so that they do not
//! shadow `T`'s own methods through `Deref`.

use core::alloc::{Allocator, Layout};
use core::borrow::Borrow;
use core::cmp::Ordering;
use core::fmt;
use core::hash::{Hash, Hasher};
use core::marker::PhantomData;
use core::ops::Deref;
use core::ptr::NonNull;

use alloc::boxed::Box;

use crate::refs::abort_process;

/// An object whose reference count is a field of its own, which C reaches
/// too: what [`CShared<Self>`] counts with. Its methods take the object's
/// pointer rather than `&self`, so a type that C writes beyond the count,
/// and so is not [`CShareable`], is never covered by a reference.
///
/// The clauses below are those of [`CRefClone`](crate::CRefClone), which
/// counts C-allocated objects, plus the allocation and the initial count,
/// which are Rust's here.
///
/// # Safety
///
/// - **Initial count.** [`c_init`](Self::c_init) sets the count to exactly
///   one reference, held by nobody else, and clears an owner field.
///   [`CShared`] calls it on every object it adopts from a `Box`, so the
///   count a value carried before is never trusted.
/// - **Taking a reference.** A `true` from [`c_up_ref`](Self::c_up_ref) means
///   the count was incremented, with the meaning C's own increment gives the
///   field, so the object owes one more [`c_down_ref`](Self::c_down_ref). A
///   `false` means nothing changed. **The count never wraps:** safe code can
///   clone a `CShared` and leak the clone as often as it likes, so
///   `c_up_ref` returns `false` before the count would wrap, unless the count
///   is too wide to wrap within a process's lifetime (64 bits). An unchecked
///   32-bit count wraps after 2^32 leaked clones, within seconds.
/// - **Dropping a reference.** `c_down_ref` drops one reference; a reference
///   either side takes is one the other side can drop. It returns `true`
///   exactly when the caller dropped the last reference that keeps the object
///   alive, so that nothing, Rust or C, is left to reach it: at most once per
///   object. A count with an owner answers `false` while the owner holds the
///   object, reading the owner after the decrement with at least `Acquire`.
/// - **Ordering**, as `Arc`'s: every decrement that drops a reference
///   (`c_down_ref`, and C's release) is at least a `Release` operation (an
///   undo of a refused increment, which drops none, is not one), and a `c_down_ref` that returns `true` performs an
///   `Acquire` (the decrement itself, or a fence after it) before it returns,
///   so that every use of the object through another reference
///   happens-before its release. [`c_is_sole_owner`](Self::c_is_sole_owner)
///   reads the count with at least `Acquire`, so that a drop on another
///   thread happens-before the `&mut Self` it grants. **No reference
///   covering more than the atomic count is live across or after a
///   decrement**: once the count drops, another thread's `c_is_sole_owner`
///   may grant `&mut Self`, or its `c_down_ref` may free the object, while
///   this call is still running. So the decrement goes through a reference
///   to the atomic alone (`&AtomicI32`, projected from the pointer), as
///   `Arc`'s does, not through a `&self` method of a count struct with
///   padding or other fields, whose argument stays protected until the
///   method returns. If `Self` is `Send` or
///   `Sync`, every change of the count is atomic with respect to C's and to
///   each other's.
/// - **Exclusive access.** A `true` from `c_is_sole_owner` means the caller's
///   reference is the only way anything, Rust or C, reaches the object, now
///   or later: every reference is counted, weak ones and any pointer C keeps
///   and could count later. Through the `&mut Self` that
///   [`CShared::get_mut`] then grants, safe code can store any `Self` value
///   it can make or move (`mem::replace`, `mem::swap`). So **every value of
///   the count's type that safe code anywhere can create or obtain holds one
///   reference and no owner**: its safe constructors make that state only,
///   it has no `Default` or `Clone` to another state, and whatever changes
///   it is `unsafe`. A private or crate-private constructor is still safe
///   code: one that makes a count of 0, or takes the count as an argument,
///   must be `unsafe` (or the impl keeps the provided `c_is_sole_owner`).
///   [`c_init`](Self::c_init) resetting the count at adoption does not
///   help here, as nothing resets it after a store through `get_mut`. An
///   impl that cannot say so keeps the provided `c_is_sole_owner`, which
///   answers `false`.
/// - **No safe access to the count.** Safe code that reaches `&Self` (through
///   [`CShareable`]'s `Deref`) or `&mut Self` cannot change the count or the
///   owner: whatever changes them is `unsafe`.
/// - **Allocation.** [`Alloc`](Self::Alloc) is stateless and interchangeable:
///   a zero-sized type (checked at compile time) whose every instance,
///   including a fresh `Default`, deallocates any other instance's blocks,
///   and whose drop frees nothing (`Global`, or a zero-sized C allocation
///   family such as a [`CAllocator`](crate::CAllocator) over a unit policy).
/// - **Release.** [`c_release`](Self::c_release) frees an object for which
///   `c_down_ref` returned `true`, and is what C's own release does then. It
///   frees every object [`CShared::from_box`] and [`CShared::try_new`] can
///   adopt, which live in a `Box<Self, Self::Alloc>`: the provided one drops
///   that box; one that replaces it still frees such a box, and any object
///   the binding adopts unsafely with [`CShared::from_raw`]. If it unwinds,
///   what it has not freed yet leaks; nothing is freed twice.
/// - None of the methods forms a reference covering more of the object than
///   the fields they use, unless `Self` is [`CShareable`].
pub unsafe trait CCounted: Sized {
    /// The allocator the object lives in: zero-sized and stateless (see the
    /// contract), which the provided [`c_release`](Self::c_release) frees it
    /// to, and [`CShared::from_box`] adopts it from.
    ///
    /// An allocator with state fails to build where `CShared` uses it:
    ///
    /// ```compile_fail,E0080
    /// use core::alloc::{AllocError, Allocator, Layout};
    /// use core::ptr::NonNull;
    /// use ffibox::{CCounted, CShared};
    /// #[derive(Default)]
    /// struct Arena(usize); // a handle: not zero-sized
    /// unsafe impl Allocator for Arena {
    ///     fn allocate(&self, l: Layout) -> Result<NonNull<[u8]>, AllocError> {
    ///         std::alloc::Global.allocate(l)
    ///     }
    ///     unsafe fn deallocate(&self, p: NonNull<u8>, l: Layout) {
    ///         unsafe { std::alloc::Global.deallocate(p, l) }
    ///     }
    /// }
    /// struct Obj(u32);
    /// unsafe impl CCounted for Obj {
    ///     type Alloc = Arena;
    ///     unsafe fn c_init(_: NonNull<Self>) {}
    ///     unsafe fn c_up_ref(_: NonNull<Self>) -> bool { false }
    ///     unsafe fn c_down_ref(_: NonNull<Self>) -> bool { true }
    /// }
    /// let _ = CShared::try_new(Obj(1));
    /// ```
    type Alloc: Allocator + Default;

    /// Set the count of a fresh object to one reference, and clear its
    /// owner.
    ///
    /// # Safety
    ///
    /// `this` addresses a live object that nothing else reaches.
    unsafe fn c_init(this: NonNull<Self>);

    /// Take one more counted reference (C's increment); `false`, changing
    /// nothing, if the count is full.
    ///
    /// # Safety
    ///
    /// `this` addresses a live object on which the caller holds a reference.
    unsafe fn c_up_ref(this: NonNull<Self>) -> bool;

    /// Drop one counted reference; `true` if the object must now be
    /// released.
    ///
    /// # Safety
    ///
    /// `this` addresses a live object; the caller gives up a reference it
    /// holds, and does not reach the object afterwards through it.
    unsafe fn c_down_ref(this: NonNull<Self>) -> bool;

    /// Whether the caller's reference is the only one, read with `Acquire`.
    /// The provided one answers `false`, which only withholds
    /// [`CShared::get_mut`]. Named as [`CRefClone`](crate::CRefClone)'s.
    ///
    /// # Safety
    ///
    /// `this` addresses a live object on which the caller holds a reference.
    #[inline]
    unsafe fn c_is_sole_owner(this: NonNull<Self>) -> bool {
        let _ = this;
        false
    }

    /// Free an object whose last reference [`c_down_ref`](Self::c_down_ref)
    /// dropped: drop it, then free its storage. The provided one drops the
    /// `Box<Self, Self::Alloc>` it lives in.
    ///
    /// # Safety
    ///
    /// `c_down_ref` returned `true` for `this`, and nothing reaches the
    /// object again.
    #[inline]
    unsafe fn c_release(this: NonNull<Self>) {
        let () = stateless_alloc::<Self>();
        // SAFETY: per the trait's contract, the object lives in a
        // `Box<Self, Self::Alloc>`, whose allocator any instance stands in
        // for, and which nothing reaches any more.
        drop(unsafe { Box::from_raw_in(this.as_ptr(), Self::Alloc::default()) });
    }
}

/// Fails the build for an `Alloc` with state.
const fn stateless_alloc<T: CCounted>() {
    struct Check<T>(PhantomData<T>);
    impl<T: CCounted> Check<T> {
        const ZST: () = assert!(
            core::mem::size_of::<T::Alloc>() == 0,
            "ffibox: CCounted::Alloc is a zero-sized, stateless allocator"
        );
    }
    Check::<T>::ZST
}

/// A [`CCounted`] object that a shared reference may cover while it is
/// shared, with C or other holders: [`CShared`] derefs to it, as `Arc<T>`
/// does.
///
/// # Safety
///
/// - While any counted reference to the object lives, every field that C, or
///   a holder through `&Self`, writes is interior-mutable: an atomic, or an
///   `UnsafeCell` behind the type's own synchronisation. Nothing moves or
///   frees a field while a reference lives.
/// - `Sync` is the type's own: it states that those writes are synchronised
///   across threads. If `Self` is not `Sync`, C writes it only on the thread
///   that holds the Rust references, and not while a `&Self` is in use there
///   (no C call runs during a borrow).
pub unsafe trait CShareable: CCounted {}

/// One counted reference to a Rust-allocated object whose count C shares:
/// `Arc<T>`, where the count is the object's own field ([`CCounted`]).
///
/// Cloning takes a reference ([`c_up_ref`](CCounted::c_up_ref), aborting if
/// the count is full, as `Arc` does); dropping drops one, and the last frees
/// the object ([`c_release`](CCounted::c_release)), whether the last holder
/// is Rust or C. A reference goes to C with [`into_raw`](Self::into_raw) and
/// comes back with [`from_raw`](Self::from_raw); C's own release routine is
/// [`release`](Self::release), behind the binding's export.
///
/// It derefs to `T` when `T` is [`CShareable`], and then compares, hashes and
/// displays as `T`, as `Arc` does. A type C writes beyond its count, without
/// interior mutability, is reached through [`as_ptr`](Self::as_ptr) and the
/// binding's own handles, never as `&T`:
///
/// ```compile_fail,E0614
/// # use core::ptr::NonNull;
/// # use core::sync::atomic::{AtomicI32, Ordering};
/// # use ffibox::counted::{CCounted, CShared};
/// # #[repr(C)] pub struct Index { rc: AtomicI32, entries: usize }
/// # unsafe impl CCounted for Index {
/// #     type Alloc = std::alloc::Global;
/// #     unsafe fn c_init(this: NonNull<Self>) {}
/// #     unsafe fn c_up_ref(this: NonNull<Self>) -> bool { false }
/// #     unsafe fn c_down_ref(this: NonNull<Self>) -> bool { true }
/// # }
/// let ix = CShared::from_box(Box::new(Index { rc: AtomicI32::new(1), entries: 0 }));
/// let _ = (*ix).entries; // not `CShareable`: C writes `entries`
/// ```
///
/// It counts only through a [`CCounted`] impl, which states how C counts:
///
/// ```compile_fail,E0277
/// use ffibox::CShared;
/// struct Plain(u32);
/// let _ = CShared::from_box(Box::new(Plain(1)));
/// ```
///
/// The exclusive borrow excludes the shared one, as for `Arc::get_mut`:
///
/// ```compile_fail,E0502
/// # use core::ptr::NonNull;
/// # use core::sync::atomic::AtomicI32;
/// # use ffibox::counted::{CCounted, CShareable, CShared};
/// # pub struct S(AtomicI32);
/// # unsafe impl CCounted for S {
/// #     type Alloc = std::alloc::Global;
/// #     unsafe fn c_init(this: NonNull<Self>) {}
/// #     unsafe fn c_up_ref(this: NonNull<Self>) -> bool { false }
/// #     unsafe fn c_down_ref(this: NonNull<Self>) -> bool { true }
/// # }
/// # unsafe impl CShareable for S {}
/// let mut s = CShared::from_box(Box::new(S(AtomicI32::new(0))));
/// let r: &S = &s;
/// let m = CShared::get_mut(&mut s);
/// let _ = (r, m);
/// ```
///
/// `Send` and `Sync` on `Arc`'s terms, `T: Send + Sync`, so a type C uses
/// on one thread at a time stays on its thread, and a `Send` type that is not
/// `Sync` is not shared:
///
/// ```compile_fail,E0277
/// # use core::cell::Cell;
/// # use core::ptr::NonNull;
/// # use ffibox::counted::{CCounted, CShared};
/// # #[repr(C)] pub struct Walk { rc: Cell<i32> }
/// # unsafe impl CCounted for Walk {
/// #     type Alloc = std::alloc::Global;
/// #     unsafe fn c_init(this: NonNull<Self>) {}
/// #     unsafe fn c_up_ref(this: NonNull<Self>) -> bool { false }
/// #     unsafe fn c_down_ref(this: NonNull<Self>) -> bool { true }
/// # }
/// fn sync<T: Sync>(_: &T) {}
/// sync(&CShared::from_box(Box::new(Walk { rc: Cell::new(1) })));
/// ```
///
/// `#[repr(transparent)]` over the pointer: `Option<CShared<T>>` is a
/// nullable `T *` holding one reference.
#[repr(transparent)]
#[must_use = "dropping a CShared drops its reference"]
pub struct CShared<T: CCounted> {
    ptr: NonNull<T>,
    _owns: PhantomData<T>,
}

// SAFETY: as `Arc`: clones on other threads share `&T` (`T: Sync`) and the
// last one may free the object on any thread (`T: Send`); `CCounted`'s
// contract makes the count atomic for a `Send` or `Sync` type.
unsafe impl<T: CCounted + Send + Sync> Send for CShared<T> {}
// SAFETY: as above.
unsafe impl<T: CCounted + Send + Sync> Sync for CShared<T> {}

impl<T: CCounted> CShared<T> {
    /// Share a boxed object, whose count `c_init` sets to one reference,
    /// which becomes this `CShared`: `Arc::from(Box<T>)`, without copying.
    #[inline]
    pub fn from_box(value: Box<T, T::Alloc>) -> Self {
        let () = stateless_alloc::<T>();
        // The allocator is zero-sized and interchangeable (the contract): it
        // is rebuilt from `Default` to free the object.
        let (ptr, _alloc) = Box::into_raw_with_allocator(value);
        // SAFETY: from a box, so non-null.
        let ptr = unsafe { NonNull::new_unchecked(ptr) };
        // SAFETY: the box was ours alone, and nothing else reaches it yet.
        unsafe { T::c_init(ptr) };
        Self {
            ptr,
            _owns: PhantomData,
        }
    }

    /// Allocate `value` in [`T::Alloc`](CCounted::Alloc) and share it:
    /// `Arc::new`, returning the value if the allocation fails.
    pub fn try_new(value: T) -> Result<Self, T> {
        let () = stateless_alloc::<T>();
        let alloc = T::Alloc::default();
        let Ok(block) = alloc.allocate(Layout::new::<T>()) else {
            return Err(value);
        };
        let ptr = block.cast::<T>();
        // SAFETY: a fresh block with `T`'s layout.
        unsafe { ptr.as_ptr().write(value) };
        // SAFETY: an initialised `T` in a block `alloc` allocated with its
        // layout: what a `Box<T, T::Alloc>` holds.
        Ok(Self::from_box(unsafe {
            Box::from_raw_in(ptr.as_ptr(), alloc)
        }))
    }

    /// Adopt one counted reference; `None` if `ptr` is null.
    ///
    /// # Safety
    ///
    /// `ptr` is null or addresses a live `T` (a `CShared` surrendered it, or
    /// C passes it) that [`c_release`](CCounted::c_release) frees, and the
    /// caller transfers one counted reference.
    #[inline]
    pub unsafe fn from_raw(ptr: *mut T) -> Option<Self> {
        NonNull::new(ptr).map(|ptr| Self {
            ptr,
            _owns: PhantomData,
        })
    }

    /// Take a new counted reference to an object the caller only borrows:
    /// `Arc::increment_strong_count` then `Arc::from_raw`, as for a pointer C
    /// lends. `None` if `ptr` is null. Aborts if the count is full.
    ///
    /// # Safety
    ///
    /// `ptr` is null or addresses a live `T` that `c_release` frees, kept
    /// alive during the call by a reference someone holds.
    #[inline]
    pub unsafe fn from_borrowed(ptr: *mut T) -> Option<Self> {
        let ptr = NonNull::new(ptr)?;
        // SAFETY: a live object, kept alive by the caller's borrow.
        if !unsafe { T::c_up_ref(ptr) } {
            abort_process();
        }
        Some(Self {
            ptr,
            _owns: PhantomData,
        })
    }

    /// The object's pointer, for C to use while `this` lives. The reference
    /// stays with `this`.
    #[inline]
    #[must_use]
    pub fn as_ptr(this: &Self) -> *mut T {
        this.ptr.as_ptr()
    }

    /// Give the reference up, to C or to a later
    /// [`from_raw`](Self::from_raw).
    #[inline]
    #[must_use = "the returned pointer holds a reference that must be released"]
    pub fn into_raw(this: Self) -> *mut T {
        let ptr = this.ptr.as_ptr();
        core::mem::forget(this);
        ptr
    }

    /// Whether two references are to the same object, like `Arc::ptr_eq`.
    #[inline]
    #[must_use]
    pub fn ptr_eq(this: &Self, other: &Self) -> bool {
        this.ptr == other.ptr
    }

    /// The object, exclusively, when this is its only reference
    /// ([`c_is_sole_owner`](CCounted::c_is_sole_owner)), like
    /// `Arc::get_mut`.
    #[inline]
    pub fn get_mut(this: &mut Self) -> Option<&mut T> {
        // SAFETY: this reference keeps the object alive.
        if unsafe { T::c_is_sole_owner(this.ptr) } {
            // SAFETY: nothing else reaches the object (`c_is_sole_owner`'s
            // contract, with `Acquire`), and `&mut this` keeps this reference
            // from being cloned while the borrow lives.
            Some(unsafe { this.ptr.as_mut() })
        } else {
            None
        }
    }

    /// Take a reference for C, which holds `ptr` already:
    /// `Arc::increment_strong_count`, C's increment. Aborts if the count is
    /// full.
    ///
    /// # Safety
    ///
    /// `ptr` addresses a live `T` on which the caller holds a reference.
    #[inline]
    pub unsafe fn increment_count(ptr: *mut T) {
        // SAFETY: per this function's contract; non-null as it is live.
        if !unsafe { T::c_up_ref(NonNull::new_unchecked(ptr)) } {
            abort_process();
        }
    }

    /// Drop a reference C holds, freeing the object if it was the last:
    /// `Arc::decrement_strong_count`.
    ///
    /// # Safety
    ///
    /// `ptr` addresses a live `T`, and the caller gives up a reference it
    /// holds.
    #[inline]
    pub unsafe fn decrement_count(ptr: *mut T) {
        // SAFETY: per this function's contract, a reference to adopt.
        drop(unsafe { Self::from_raw(ptr) });
    }

    /// C's release routine (`foo_free(foo *)`): drops the reference C gives
    /// up, freeing the object if it was the last; NULL is ignored, as by
    /// `free`. A binding calls it from its export, inside the export's panic
    /// barrier, which then catches a panic in `T`'s `Drop` or `c_release`.
    ///
    /// # Safety
    ///
    /// `ptr` is NULL, or addresses a live `T` on which the caller gives up a
    /// reference it holds.
    #[inline]
    pub unsafe fn release(ptr: *mut T) {
        // SAFETY: per this function's contract.
        drop(unsafe { Self::from_raw(ptr) });
    }
}

impl<T: CCounted> Clone for CShared<T> {
    /// Another reference to the same object, through
    /// [`c_up_ref`](CCounted::c_up_ref); aborts if the count is full, as
    /// `Arc::clone` does.
    #[inline]
    fn clone(&self) -> Self {
        // SAFETY: this reference keeps the object alive.
        if !unsafe { T::c_up_ref(self.ptr) } {
            abort_process();
        }
        Self {
            ptr: self.ptr,
            _owns: PhantomData,
        }
    }
}

impl<T: CCounted> Drop for CShared<T> {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: this value's reference, given up once.
        if unsafe { T::c_down_ref(self.ptr) } {
            // SAFETY: the last reference: nothing reaches the object.
            unsafe { T::c_release(self.ptr) };
        }
    }
}

impl<T: CCounted> From<Box<T, T::Alloc>> for CShared<T> {
    /// [`from_box`](Self::from_box).
    #[inline]
    fn from(value: Box<T, T::Alloc>) -> Self {
        Self::from_box(value)
    }
}

impl<T: CShareable> Deref for CShared<T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        // SAFETY: this reference keeps the object alive, and `CShareable`
        // lets a shared reference cover it while it is shared.
        unsafe { self.ptr.as_ref() }
    }
}

impl<T: CShareable> AsRef<T> for CShared<T> {
    #[inline]
    fn as_ref(&self) -> &T {
        self
    }
}

impl<T: CShareable> Borrow<T> for CShared<T> {
    #[inline]
    fn borrow(&self) -> &T {
        self
    }
}

impl<T: CShareable + PartialEq> PartialEq for CShared<T> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}
impl<T: CShareable + Eq> Eq for CShared<T> {}

impl<T: CShareable + PartialOrd> PartialOrd for CShared<T> {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        (**self).partial_cmp(&**other)
    }
}
impl<T: CShareable + Ord> Ord for CShared<T> {
    #[inline]
    fn cmp(&self, other: &Self) -> Ordering {
        (**self).cmp(&**other)
    }
}

impl<T: CShareable + Hash> Hash for CShared<T> {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        (**self).hash(state);
    }
}

impl<T: CShareable + fmt::Display> fmt::Display for CShared<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&**self, f)
    }
}

/// The pointer, for every `T`: a `T` that is not [`CShareable`] cannot be
/// read through a shared reference.
impl<T: CCounted> fmt::Debug for CShared<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("CShared").field(&self.ptr.as_ptr()).finish()
    }
}

impl<T: CCounted> fmt::Pointer for CShared<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Pointer::fmt(&self.ptr.as_ptr(), f)
    }
}

const _: () = {
    #[repr(C)]
    struct Probe(u64);
    // SAFETY: never used; only measured.
    unsafe impl CCounted for Probe {
        type Alloc = alloc::alloc::Global;
        unsafe fn c_init(_: NonNull<Self>) {}
        unsafe fn c_up_ref(_: NonNull<Self>) -> bool {
            false
        }
        unsafe fn c_down_ref(_: NonNull<Self>) -> bool {
            false
        }
    }
    assert!(core::mem::size_of::<CShared<Probe>>() == core::mem::size_of::<*mut Probe>());
    assert!(core::mem::size_of::<Option<CShared<Probe>>>() == core::mem::size_of::<*mut Probe>());
};
