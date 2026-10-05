//! Shared ownership over a C refcount: [`CArc`], plus the C lock some shared
//! objects carry — [`CGuard`], taken through [`CArc::lock`] — and
//! [`CGuardedRef`], a borrow reached only through such a lock.
//!
//! A `CArc` is `Arc<T>` for a `T` whose count lives in the C object. It hands
//! out three handles, each sound for its own reason:
//!
//! - [`as_ref`](CArc::as_ref) — the shared handle, always: state nobody
//!   writes once the object is shared, and C routines that lock internally.
//! - [`get_mut`](CArc::get_mut) — the exclusive handle, when the refcount
//!   proves this reference sole; [`make_mut`](CArc::make_mut) copies first.
//! - [`lock`](CArc::lock) — when the layout type is [`CGuarded`]: a
//!   [`CGuard`] holding the C lock, whose [`Locked`](CGuarded::Locked) handle
//!   reaches the state that lock protects. The common C shape — a lock over a
//!   cache or a list, not the whole object — is `Arc<T>` with a `Mutex` field,
//!   not `Arc<Mutex<T>>`.
//!
//! [`CGuardedRef`] is the whole-object case, `&Mutex<T>`: a [`CGuardedAll`]
//! object, typically a C global under its C lock, borrowed for `'static`, with
//! no unlocked path at all.
//!
//! ```
//! use core::ptr::addr_of;
//! use ffibox::{define_ctype, impl_cdrop, impl_crefclone, CArc};
//!
//! mod ffi {
//!     use core::sync::atomic::{AtomicUsize, Ordering};
//!     #[repr(C)]
//!     pub struct session_st { pub rc: AtomicUsize, pub id: u32 }
//!     // Stand-ins for a C library's constructor and refcount pair.
//!     pub fn session_new(id: u32) -> *mut session_st {
//!         Box::into_raw(Box::new(session_st { rc: AtomicUsize::new(1), id }))
//!     }
//!     pub unsafe fn session_up_ref(p: *mut session_st) {
//!         unsafe { (*p).rc.fetch_add(1, Ordering::Relaxed) };
//!     }
//!     pub unsafe fn session_free(p: *mut session_st) {
//!         if unsafe { (*p).rc.fetch_sub(1, Ordering::AcqRel) } == 1 {
//!             drop(unsafe { Box::from_raw(p) });
//!         }
//!     }
//!     pub unsafe fn session_is_sole(p: *mut session_st) -> bool {
//!         unsafe { (*p).rc.load(Ordering::Acquire) == 1 }
//!     }
//! }
//!
//! define_ctype!(Session, SessionRef, SessionMut, ffi::session_st);
//! impl SessionRef<'_> {
//!     pub fn id(&self) -> u32 {
//!         // SAFETY: a read through the raw pointer; no reference is formed.
//!         unsafe { addr_of!((*self.as_ptr()).id).read() }
//!     }
//! }
//!
//! #[derive(Clone, Copy, Debug, Default)]
//! pub struct SessionUnref;
//! impl_cdrop!(SessionUnref, Session, ffi::session_free);
//! impl_crefclone!(SessionUnref, Session, ffi::session_up_ref, sole = ffi::session_is_sole);
//! pub type SessionArc = CArc<Session, SessionUnref>;
//!
//! // SAFETY: a fresh session, whose one reference `session_free` releases.
//! let mut a = unsafe { SessionArc::from_c(ffi::session_new(7)) }.unwrap();
//! let b = a.clone();                       // session_up_ref
//! assert!(SessionArc::ptr_eq(&a, &b));
//! assert_eq!(b.as_ref().id(), 7);
//! assert!(a.get_mut().is_none());          // shared: no exclusive handle
//! drop(b);
//! assert!(a.get_mut().is_some());          // sole again
//! ```

use core::fmt;
use core::marker::PhantomData;
use core::ptr::NonNull;

use crate::refs::{abort_process, disarm, handle_mut, handle_ref, CBorrowedPtr, CBox};
#[allow(unused_imports)] // doc links
use crate::traits::LockWhole;
use crate::traits::{CCell, CDrop, CDupClone, CGuarded, CGuardedAll, CRefClone, LockFields};

// ---------------------------------------------------------------------------
// CArc<T, D> — a shared reference to a refcounted C object
// ---------------------------------------------------------------------------

/// One counted reference to a refcounted C object, released on drop by the
/// policy's down-ref and cloned through its up_ref ([`CRefClone`]) — an
/// `Arc` whose count lives in the C object.
///
/// Every clone points at the same object, so a `CArc` hands out the shared
/// handle ([`as_ref`](Self::as_ref)) to all of them. Exclusive access needs the
/// refcount to prove this reference sole ([`get_mut`](Self::get_mut)), or a
/// private copy ([`make_mut`](Self::make_mut)). State that every holder
/// mutates after the object is shared is reached through the C lock protecting
/// it ([`lock`](Self::lock), when `T` is [`CGuarded`]).
///
/// The policy needs only [`CDrop`] to hold a reference; a drop-only share (a
/// counted reference C handed over, with no up_ref to call) is a `CArc`
/// without `Clone`, never a [`CBox`].
///
/// `Send` / `Sync` on `Arc`'s terms: `T: Send + Sync`, plus the policy's.
#[repr(C)]
#[must_use = "dropping a CArc releases its reference"]
pub struct CArc<T, D: CDrop<T>> {
    ptr: NonNull<T>,
    policy: D,
}

impl<T, D: CDrop<T>> CArc<T, D> {
    /// Adopt one counted reference under `D::default()`; `None` if null.
    ///
    /// # Safety
    ///
    /// `ptr` must be null or address a live `T`, and the caller transfers one
    /// counted reference that `D::c_drop` releases.
    #[inline]
    pub unsafe fn from_raw(ptr: *mut T) -> Option<Self>
    where
        D: Default,
    {
        // SAFETY: the caller upholds `from_raw`'s contract.
        unsafe { Self::from_raw_with(ptr, D::default()) }
    }

    /// Adopt one counted reference under `policy`; `None` if null.
    ///
    /// # Safety
    ///
    /// As [`from_raw`](Self::from_raw), with `policy` releasing it.
    #[inline]
    pub unsafe fn from_raw_with(ptr: *mut T, policy: D) -> Option<Self> {
        NonNull::new(ptr).map(|ptr| Self { ptr, policy })
    }

    /// Raw pointer for passing to C. The reference is retained.
    #[inline]
    #[must_use]
    pub fn as_ptr(&self) -> *mut T {
        self.ptr.as_ptr()
    }

    /// The policy that will release the reference.
    #[inline]
    #[must_use]
    pub fn policy(&self) -> &D {
        &self.policy
    }

    /// Give up the reference without releasing it.
    #[inline]
    #[must_use = "the returned pointer owns a reference and must be released"]
    pub fn into_raw(self) -> *mut T {
        self.into_raw_with().0
    }

    /// As [`into_raw`](Self::into_raw), also returning the policy.
    #[inline]
    #[must_use = "the returned pointer owns a reference and must be released"]
    pub fn into_raw_with(self) -> (*mut T, D) {
        // SAFETY: each field is read out exactly once and `self` is not
        // dropped.
        unsafe { disarm(self, |o| (o.ptr.as_ptr(), core::ptr::read(&o.policy))) }
    }
}

impl<T, D: CDrop<T>> CArc<T, D> {
    /// Whether two arcs reference the same object, like
    /// [`Arc::ptr_eq`](https://doc.rust-lang.org/std/sync/struct.Arc.html#method.ptr_eq).
    #[inline]
    #[must_use]
    pub fn ptr_eq(this: &Self, other: &Self) -> bool {
        this.ptr == other.ptr
    }
}

impl<T: CCell, D: CDrop<T>> CArc<T, D> {
    /// [`from_raw`](Self::from_raw) from the C type's pointer.
    ///
    /// # Safety
    ///
    /// As [`from_raw`](Self::from_raw).
    #[inline]
    pub unsafe fn from_c(ptr: *mut T::C) -> Option<Self>
    where
        D: Default,
    {
        // SAFETY: `T` is layout-compatible with `T::C` per `CCell`; the caller
        // upholds the rest.
        unsafe { Self::from_raw(ptr.cast()) }
    }

    /// [`from_raw_with`](Self::from_raw_with) from the C type's pointer.
    ///
    /// # Safety
    ///
    /// As [`from_raw`](Self::from_raw), with `policy` releasing it.
    #[inline]
    pub unsafe fn from_c_with(ptr: *mut T::C, policy: D) -> Option<Self> {
        // SAFETY: as `from_c`.
        unsafe { Self::from_raw_with(ptr.cast(), policy) }
    }

    /// [`into_raw`](Self::into_raw) as the C type's pointer.
    #[inline]
    #[must_use = "the returned pointer owns a reference and must be released"]
    pub fn into_c(self) -> *mut T::C {
        self.into_raw().cast()
    }

    /// [`as_ptr`](Self::as_ptr) as the C type's pointer.
    #[inline]
    #[must_use]
    pub fn as_c_ptr(&self) -> *mut T::C {
        self.as_ptr().cast()
    }

    /// Shared handle to the object — the getters.
    #[inline]
    #[must_use]
    pub fn as_ref(&self) -> T::Ref<'_> {
        // SAFETY: our reference keeps the object live for the borrow, and no
        // clone can hand out a write path (see the type docs).
        unsafe { handle_ref(self.ptr) }
    }
}

impl<T: CCell, D: CRefClone<T>> CArc<T, D> {
    /// Exclusive handle, if this is provably the only reference
    /// ([`CRefClone::c_is_sole_owner`]); `None` otherwise.
    #[inline]
    #[must_use]
    pub fn get_mut(&mut self) -> Option<T::Mut<'_>> {
        // SAFETY: our reference keeps the object live; per the contract, a
        // `true` means no other reference can reach it, and `&mut self` keeps
        // this one from being cloned meanwhile.
        unsafe { sole_mut(self.ptr, &self.policy) }
    }

    /// Exclusive handle, copying the object first through [`CDupClone`] unless
    /// this is provably the only reference. `None` if the copy failed.
    #[inline]
    pub fn try_make_mut(&mut self) -> Option<T::Mut<'_>>
    where
        D: CDupClone<T>,
    {
        // SAFETY: we hold a live reference. No clone writes outside the
        // object's lock, and a `CDupClone` for a `CGuarded` type takes that
        // lock itself (`CGuarded`'s contract), so the copy reads a quiescent
        // object.
        let copy = unsafe { copy_unless_sole(self.ptr, &self.policy) }?;
        // SAFETY: `copy` came from `copy_unless_sole` on our reference.
        unsafe { adopt_copy(&mut self.ptr, &self.policy, copy) };
        // SAFETY: `self.ptr` is now the sole reference, exclusively borrowed.
        Some(unsafe { handle_mut(self.ptr) })
    }

    /// As [`try_make_mut`](Self::try_make_mut), aborting if the copy fails.
    #[inline]
    pub fn make_mut(&mut self) -> T::Mut<'_>
    where
        D: CDupClone<T>,
    {
        if self.try_make_mut().is_none() {
            abort_process();
        }
        // SAFETY: `try_make_mut` left `self.ptr` the sole reference.
        unsafe { handle_mut(self.ptr) }
    }
}

impl<T: CGuarded<Scope = LockFields>, D: CDrop<T>> CArc<T, D> {
    /// Take the C lock protecting part of the object: a guard handing out the
    /// [`Locked`](CGuarded::Locked) handle, unlocking on drop. Takes `&self`:
    /// the lock, not the borrow, makes the access exclusive, so any clone on
    /// any thread may lock in turn.
    ///
    /// A [`CGuardedAll`] type ([`LockWhole`]) is rejected at compile time:
    /// its `Locked` handle is `Mut`, and other clones still reach
    /// [`as_ref`](Self::as_ref) unlocked. Such an object belongs in a
    /// [`CGuardedRef`].
    ///
    /// ```compile_fail,E0271
    /// # use core::ptr::NonNull;
    /// # use ffibox::{define_ctype, impl_cguarded, CArc, CDrop};
    /// # #[repr(C)] pub struct reg_st { x: u8 }
    /// # unsafe fn reg_lock(_: *mut reg_st) {}
    /// # unsafe fn reg_unlock(_: *mut reg_st) {}
    /// define_ctype!(Reg, RegRef, RegMut, reg_st);
    /// impl_cguarded!(Reg, all, lock = reg_lock, unlock = reg_unlock);
    /// # #[derive(Default)] struct Unref;
    /// # unsafe impl CDrop<Reg> for Unref { unsafe fn c_drop(&self, _: NonNull<Reg>) {} }
    ///
    /// fn race(a: &CArc<Reg, Unref>) {
    ///     let _g = a.lock(); // a `Mut` handle while clones read `as_ref()`
    /// }
    /// ```
    ///
    /// # Panics
    ///
    /// If the C lock call reports failure. A second `lock` on a thread already
    /// holding a guard deadlocks, as `Mutex` may — or panics, when the C lock
    /// reports the self-deadlock (`EDEADLK`) and is bound with
    /// [`impl_cguarded!`](crate::impl_cguarded)'s `ok` check.
    #[inline]
    pub fn lock(&self) -> CGuard<'_, T> {
        // SAFETY: our reference keeps the object live for the guard's borrow.
        unsafe { take_lock(self.ptr) }
    }
}

impl<T, D: CRefClone<T>> CArc<T, D> {
    /// Another reference through [`CRefClone::c_up_ref`]; `None` if it failed.
    #[inline]
    pub fn try_clone(&self) -> Option<Self>
    where
        D: Clone,
    {
        // SAFETY: our reference keeps the object live; a `true` owes one more
        // `c_drop`, which the clone's policy settles.
        unsafe { self.policy.c_up_ref(self.ptr) }.then(|| Self {
            ptr: self.ptr,
            policy: self.policy.clone(),
        })
    }

    /// The sole owner, if this is provably the only reference; `self` back
    /// otherwise.
    #[inline]
    pub fn try_into_box(self) -> Result<CBox<T, D>, Self> {
        // SAFETY: our reference keeps the object live.
        if unsafe { self.policy.c_is_sole_owner(self.ptr) } {
            let (ptr, policy) = self.into_raw_with();
            // SAFETY: the reference is the only one, so the box is the sole
            // owner, and the policy releases it.
            Ok(unsafe { CBox::from_raw_with(ptr, policy) }.unwrap_or_else(|| abort_process()))
        } else {
            Err(self)
        }
    }
}

impl<T, D: CRefClone<T> + Clone> Clone for CArc<T, D> {
    #[inline]
    fn clone(&self) -> Self {
        // Abort on a failed up_ref (an overflowing count), as `Arc` does.
        match self.try_clone() {
            Some(a) => a,
            None => abort_process(),
        }
    }
}

impl<T, D: CDrop<T>> From<CBox<T, D>> for CArc<T, D> {
    /// Share a sole owner: its one reference becomes the first `CArc`.
    #[inline]
    fn from(b: CBox<T, D>) -> Self {
        let (ptr, policy) = b.into_raw_with();
        // SAFETY: `ptr` came from a live box, so it is non-null, and the box
        // owned one reference.
        let ptr = unsafe { NonNull::new_unchecked(ptr) };
        Self { ptr, policy }
    }
}

impl<T, D: CRefClone<T>> TryFrom<CArc<T, D>> for CBox<T, D> {
    type Error = CArc<T, D>;

    /// [`CArc::try_into_box`]: the sole owner, or the arc back if it is not
    /// provably the only reference — like [`Arc::try_unwrap`](https://doc.rust-lang.org/std/sync/struct.Arc.html#method.try_unwrap).
    #[inline]
    fn try_from(arc: CArc<T, D>) -> Result<Self, Self::Error> {
        arc.try_into_box()
    }
}

impl<T, D: CDrop<T>> Drop for CArc<T, D> {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: we hold one counted reference; the policy releases it once.
        unsafe { self.policy.c_drop(self.ptr) }
    }
}

impl<T, D: CDrop<T>> fmt::Debug for CArc<T, D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("CArc").field(&self.ptr.as_ptr()).finish()
    }
}

impl<T, D: CDrop<T>> fmt::Pointer for CArc<T, D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Pointer::fmt(&self.ptr.as_ptr(), f)
    }
}

// SAFETY: as `Arc`: clones on other threads share `&T` access (`T: Sync`), and
// the last one may release the object on any thread (`T: Send`). The policy
// travels with each reference and is reached through `&self`.
unsafe impl<T: Send + Sync, D: CDrop<T> + Send + Sync> Send for CArc<T, D> {}
// SAFETY: as above.
unsafe impl<T: Send + Sync, D: CDrop<T> + Send + Sync> Sync for CArc<T, D> {}

// ---------------------------------------------------------------------------
// CGuardedRef<'a, T> — a borrow reached only through the object's lock
// ---------------------------------------------------------------------------

/// A borrow of an object whose lock covers all of it ([`CGuardedAll`]) and
/// that nothing in Rust owns or releases — `&'a Mutex<T>`. There is no
/// unlocked path: [`lock`](Self::lock) returns a [`CGuard`] whose
/// [`as_locked`](CGuard::as_locked) is the object's `Mut` handle.
///
/// Typically a C **global** guarded by a C lock, borrowed for `'static`. A
/// global is never freed, so it needs no owner, only the lock:
///
/// ```ignore
/// pub type RegistryLock = CGuardedRef<'static, Registry>;
///
/// pub fn registry() -> RegistryLock {
///     // SAFETY: `ffi::registry` lives for the program, and every C access
///     // takes `registry_lock`, which `Registry`'s `CGuarded` impl binds.
///     unsafe { RegistryLock::from_ptr(addr_of_mut!(ffi::registry)) }.unwrap()
/// }
///
/// registry().lock().as_locked().add(42);
/// ```
///
/// `Copy`, like `&Mutex<T>`, and `Send` / `Sync` on its terms: `T: Send`.
/// **Invariant in `T`**, as `&Mutex<T>` is, since a copy may write:
///
/// ```compile_fail
/// # use core::marker::PhantomData;
/// # use core::ptr::NonNull;
/// # use ffibox::{CBorrowedPtr, CCell, CGuarded, CGuardedAll, CGuardedRef};
/// # #[repr(C)] pub struct holder_st { p: *const u32 }
/// # #[repr(transparent)] pub struct Holder<'x>(holder_st, PhantomData<&'x u32>);
/// # #[repr(transparent)] #[derive(Clone, Copy)]
/// # pub struct HolderRef<'a, 'x>(CBorrowedPtr<'a, Holder<'x>>);
/// # #[repr(transparent)]
/// # pub struct HolderMut<'a, 'x>(HolderRef<'a, 'x>, PhantomData<&'a mut Holder<'x>>);
/// # unsafe impl<'x> CCell for Holder<'x> {
/// #     type C = holder_st;
/// #     type Ref<'a> = HolderRef<'a, 'x> where Self: 'a;
/// #     type Mut<'a> = HolderMut<'a, 'x> where Self: 'a;
/// # }
/// # unsafe impl<'x> CGuarded for Holder<'x> {
/// #     type Locked<'a> = HolderMut<'a, 'x> where Self: 'a;
/// #     type Scope = ffibox::LockWhole;
/// #     unsafe fn c_lock(_: NonNull<Self>) -> bool { true }
/// #     unsafe fn c_unlock(_: NonNull<Self>) {}
/// # }
/// # unsafe impl<'x> CGuardedAll for Holder<'x> {}
/// // `'a`, not `'static`: a `'static` borrow would force `'s` to be `'static`
/// // too, and the shrink would be a no-op.
/// fn shrink<'a, 's>(r: CGuardedRef<'a, Holder<'static>>) -> CGuardedRef<'a, Holder<'s>> {
///     r
/// }
/// ```
pub struct CGuardedRef<'a, T: CGuardedAll> {
    ptr: NonNull<T>,
    // Invariant in `T`, as a writer's borrow must be; `NonNull` withholds
    // `Send` / `Sync` until the impls below grant them on `&Mutex<T>`'s terms.
    _borrow: PhantomData<&'a mut T>,
}

impl<T: CGuardedAll> Clone for CGuardedRef<'_, T> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}
// Copies like `&Mutex<T>`; `#[derive]` would add a spurious `T: Copy`.
impl<T: CGuardedAll> Copy for CGuardedRef<'_, T> {}

impl<'a, T: CGuardedAll> CGuardedRef<'a, T> {
    /// Borrow the object at a C pointer; `None` if null. Borrows, so it is
    /// `from_ptr`, not `from_raw`: nothing is released when the view goes.
    ///
    /// # Safety
    ///
    /// - `ptr` must be null or address a live `T::C` that outlives `'a`.
    /// - [`CGuardedAll`]'s contract must hold for this object: every access,
    ///   C's included, takes its lock.
    /// - For `'a`, nothing may reach the object's handles without the lock —
    ///   no owner, no handle built from the raw pointer.
    #[inline]
    pub unsafe fn from_ptr(ptr: *mut T::C) -> Option<Self> {
        NonNull::new(ptr.cast::<T>()).map(|ptr| Self {
            ptr,
            _borrow: PhantomData,
        })
    }

    /// Raw pointer for passing to C. Calls that touch the object's state take
    /// its lock themselves, or need a [`CGuard`] held.
    #[inline]
    #[must_use]
    pub fn as_ptr(self) -> *mut T {
        self.ptr.as_ptr()
    }

    /// [`as_ptr`](Self::as_ptr) as the C type's pointer.
    #[inline]
    #[must_use]
    pub fn as_c_ptr(self) -> *mut T::C {
        self.ptr.as_ptr().cast()
    }

    /// Take the lock: a guard handing out the `Mut` handle, unlocking on drop.
    /// By value, like a method on `&'a Mutex<T>`, so the guard keeps `'a` and
    /// `let g = registry().lock();` outlives its statement. Any copy on any
    /// thread may lock in turn: the lock, not the borrow, makes it exclusive.
    ///
    /// # Panics
    ///
    /// If the C lock call reports failure.
    #[inline]
    pub fn lock(self) -> CGuard<'a, T> {
        // SAFETY: the object outlives `'a`, and every path to it locks, per
        // `from_ptr`.
        unsafe { take_lock(self.ptr) }
    }
}

impl<T: CGuardedAll> fmt::Debug for CGuardedRef<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("CGuardedRef")
            .field(&self.ptr.as_ptr())
            .finish()
    }
}

impl<T: CGuardedAll> fmt::Pointer for CGuardedRef<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Pointer::fmt(&self.ptr.as_ptr(), f)
    }
}

// SAFETY: as `&Mutex<T>`: a copy on another thread reaches the object only
// under the lock, one thread at a time, which `T: Send` permits.
unsafe impl<T: CGuardedAll + Send> Send for CGuardedRef<'_, T> {}
// SAFETY: as above; `&CGuardedRef` only copies the view out.
unsafe impl<T: CGuardedAll + Send> Sync for CGuardedRef<'_, T> {}

// ---------------------------------------------------------------------------
// CGuard<'a, T> — a held lock
// ---------------------------------------------------------------------------

/// A held [`CGuarded`] lock, released on drop — `MutexGuard`. From
/// [`CArc::lock`] or [`CGuardedRef::lock`].
///
/// [`as_locked`](Self::as_locked) hands out the [`Locked`](CGuarded::Locked)
/// handle, exclusive over the state the lock protects (the whole object's
/// `Mut` handle for [`CGuardedAll`]); [`as_ref`](Self::as_ref) the shared
/// handle. Both borrow the guard, so neither outlives the lock:
///
/// ```compile_fail,E0505
/// # use core::ptr::NonNull;
/// # use ffibox::{define_ctype, impl_cguarded, CArc, CDrop};
/// # #[repr(C)] pub struct obj_st { x: u8 }
/// # unsafe fn obj_lock(_: *mut obj_st) {}
/// # unsafe fn obj_unlock(_: *mut obj_st) {}
/// # define_ctype!(Obj, ObjRef, ObjMut, obj_st);
/// # impl_cguarded!(Obj, ObjLocked, lock = obj_lock, unlock = obj_unlock);
/// # #[derive(Default)] struct Unref;
/// # unsafe impl CDrop<Obj> for Unref { unsafe fn c_drop(&self, _: NonNull<Obj>) {} }
/// fn escape(arc: &CArc<Obj, Unref>) {
///     let mut guard = arc.lock();
///     let handle = guard.as_locked();
///     drop(guard); // unlocks here ...
///     drop(handle); // ... so the handle may not live on
/// }
/// ```
///
/// `!Send`, like `MutexGuard`: the unlock must run on the locking thread.
///
/// **Invariant in `T`**, as `MutexGuard` is, since it hands out an exclusive
/// handle: a guard over `Holder<'static>` must not become one over
/// `Holder<'short>`, which could store a `'short` borrow in the object.
///
/// ```compile_fail
/// # use core::marker::PhantomData;
/// # use core::ptr::NonNull;
/// # use ffibox::{CBorrowedPtr, CCell, CGuard, CGuarded};
/// # #[repr(C)] pub struct holder_st { p: *const u32 }
/// # #[repr(transparent)] pub struct Holder<'x>(holder_st, PhantomData<&'x u32>);
/// # #[repr(transparent)] #[derive(Clone, Copy)]
/// # pub struct HolderRef<'a, 'x>(CBorrowedPtr<'a, Holder<'x>>);
/// # #[repr(transparent)]
/// # pub struct HolderMut<'a, 'x>(HolderRef<'a, 'x>, PhantomData<&'a mut Holder<'x>>);
/// # unsafe impl<'x> CCell for Holder<'x> {
/// #     type C = holder_st;
/// #     type Ref<'a> = HolderRef<'a, 'x> where Self: 'a;
/// #     type Mut<'a> = HolderMut<'a, 'x> where Self: 'a;
/// # }
/// # unsafe impl<'x> CGuarded for Holder<'x> {
/// #     type Locked<'a> = HolderMut<'a, 'x> where Self: 'a;
/// #     type Scope = ffibox::LockFields;
/// #     unsafe fn c_lock(_: NonNull<Self>) -> bool { true }
/// #     unsafe fn c_unlock(_: NonNull<Self>) {}
/// # }
/// fn shrink<'g, 's>(g: CGuard<'g, Holder<'static>>) -> CGuard<'g, Holder<'s>> {
///     g
/// }
/// ```
#[must_use = "dropping the guard releases the lock at once"]
pub struct CGuard<'a, T: CGuarded> {
    ptr: NonNull<T>,
    _borrow: PhantomData<(&'a mut T, *const ())>,
}

impl<T: CGuarded> CGuard<'_, T> {
    /// Shared handle to the object — the getters that need no lock. Getters
    /// for the protected state live on [`as_locked`](Self::as_locked)'s handle.
    #[inline]
    #[must_use]
    pub fn as_ref(&self) -> T::Ref<'_> {
        // SAFETY: the guard's borrow keeps the object live, and a `Ref` is
        // valid unlocked too.
        unsafe { handle_ref(self.ptr) }
    }

    /// The locked handle — getters and setters for the state the lock
    /// protects. Takes `&mut self`: the lock excludes every other holder, and
    /// the borrow makes this the only handle the guard has out.
    #[inline]
    #[must_use]
    pub fn as_locked(&mut self) -> T::Locked<'_> {
        // SAFETY: the lock is held and not recursive, so no other `Locked`
        // handle exists on any thread; the handle is bound to the guard's
        // exclusive borrow, so it dies before the unlock.
        unsafe { handle_locked(self.ptr) }
    }

    /// Raw pointer for C calls that require the lock held.
    #[inline]
    #[must_use]
    pub fn as_ptr(&self) -> *mut T {
        self.ptr.as_ptr()
    }
}

impl<T: CGuarded> Drop for CGuard<'_, T> {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: this guard took the lock, on this thread (`!Send`).
        unsafe { T::c_unlock(self.ptr) }
    }
}

impl<T: CGuarded> fmt::Debug for CGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("CGuard").field(&self.ptr.as_ptr()).finish()
    }
}

// SAFETY: sharing `&CGuard` shares only `T::Ref` access, which `T: Sync`
// permits; `as_locked` needs `&mut`.
unsafe impl<T: CGuarded + Sync> Sync for CGuard<'_, T> {}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// The exclusive handle if `policy` proves `ptr` the sole reference.
///
/// # Safety
///
/// `ptr` must be a live reference the caller holds through `&mut`, so it
/// cannot be cloned while the handle lives.
#[inline]
unsafe fn sole_mut<'a, T: CCell + 'a, D: CRefClone<T>>(
    ptr: NonNull<T>,
    policy: &D,
) -> Option<T::Mut<'a>> {
    // SAFETY: the caller keeps `ptr` live; a `true` means no other reference
    // can reach the object.
    if unsafe { policy.c_is_sole_owner(ptr) } {
        // SAFETY: sole, and exclusively borrowed by the caller.
        Some(unsafe { handle_mut(ptr) })
    } else {
        None
    }
}

/// A fresh copy of the object at `ptr` unless `policy` proves `ptr` the sole
/// reference: `Some(None)` if already sole, `Some(Some(copy))` with a copy
/// owing one `c_drop`, `None` if the copy failed.
///
/// # Safety
///
/// `ptr` must be a live reference the caller holds, and no write to the
/// object may run while the copy reads it.
#[inline]
unsafe fn copy_unless_sole<T, D: CRefClone<T> + CDupClone<T>>(
    ptr: NonNull<T>,
    policy: &D,
) -> Option<Option<NonNull<T>>> {
    // SAFETY: the caller keeps `ptr` live.
    if unsafe { policy.c_is_sole_owner(ptr) } {
        return Some(None);
    }
    // SAFETY: the caller keeps `ptr` live and quiescent; per `CDupClone` a
    // `Some` is a fresh object owing one `c_drop` of this policy.
    unsafe { policy.c_dup(ptr) }.map(Some)
}

/// Swap a copy from [`copy_unless_sole`] into `*ptr`, releasing the reference
/// it replaces. A no-op for `None` (already sole).
///
/// # Safety
///
/// `*ptr` must be a reference the caller holds, not locked by the caller,
/// and `copy` must come from `copy_unless_sole` on it.
#[inline]
unsafe fn adopt_copy<T, D: CDrop<T>>(ptr: &mut NonNull<T>, policy: &D, copy: Option<NonNull<T>>) {
    if let Some(copy) = copy {
        let old = core::mem::replace(ptr, copy);
        // SAFETY: releases the reference the caller held on the original.
        unsafe { policy.c_drop(old) };
    }
}

/// Take `ptr`'s lock and wrap it in a guard; panics if the lock fails.
///
/// # Safety
///
/// `ptr` must address a live `T` that outlives `'a`.
#[inline]
unsafe fn take_lock<'a, T: CGuarded>(ptr: NonNull<T>) -> CGuard<'a, T> {
    // SAFETY: the caller keeps `ptr` live for `'a`.
    if !unsafe { T::c_lock(ptr) } {
        lock_failed();
    }
    CGuard {
        ptr,
        _borrow: PhantomData,
    }
}

/// Build `T`'s locked handle from a pointer to it. As [`handle_mut`], for
/// [`CGuarded::Locked`].
///
/// # Safety
///
/// As [`handle_mut`]: `p` is live for `'a`, carries write provenance, and no
/// other `Locked` or `Mut` handle to the object is used while the result
/// lives.
#[inline]
unsafe fn handle_locked<'a, T: CGuarded + 'a>(p: NonNull<T>) -> T::Locked<'a> {
    // Checked at compile time for every `T` this is instantiated with: a
    // `CGuarded` impl whose handle is not pointer-sized fails the build.
    const {
        assert!(
            core::mem::size_of::<T::Locked<'a>>() == core::mem::size_of::<CBorrowedPtr<'a, T>>(),
            "CGuarded::Locked must be transparent over CBorrowedPtr",
        )
    };
    // SAFETY: the caller upholds liveness for `'a`.
    let raw = unsafe { CBorrowedPtr::<'a, T>::new(p) };
    // SAFETY: `CGuarded`'s contract makes `T::Locked<'a>` transparent over
    // `CBorrowedPtr<'a, T>` with no `Drop`; the sizes are equal (checked
    // above), so `transmute_copy` reads exactly the one pointer `raw` holds.
    unsafe { core::mem::transmute_copy(&raw) }
}

/// A C lock call reported failure. Panicking is safe here: no guard exists
/// yet, so nothing is left locked.
#[cold]
#[inline(never)]
#[track_caller]
fn lock_failed() -> ! {
    panic!("ffibox: the C object's lock call failed")
}
