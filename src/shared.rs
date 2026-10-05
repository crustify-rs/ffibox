//! Shared owners over a C refcount: [`CArc`], which hands out only shared
//! handles, and [`CGuardedArc`], which reaches the object through the C
//! object's own lock — [`CReadGuard`] for readers, [`CWriteGuard`] for a
//! writer.
//!
//! The split is `Arc<T>` versus `Arc<RwLock<T>>`. A `CArc` has no unlocked
//! write path at all, so any number of clones on any number of threads only
//! ever read. A `CGuardedArc` has no unlocked path of either kind, so a writer
//! on one clone excludes readers and writers on every other clone. Both reach
//! the exclusive handle without a lock only when the refcount proves the
//! reference sole ([`get_mut`](CArc::get_mut)), or after copying the object
//! ([`make_mut`](CArc::make_mut)).
//!
//! [`CGuardedRef`] is the guarded *borrow*: `&RwLock<T>` to `CGuardedArc`'s
//! `Arc<RwLock<T>>`. It takes the same guards over an object nothing in Rust
//! releases — typically a C global and the C lock that protects it, borrowed
//! for `'static`.
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

use crate::refs::{abort_process, disarm, handle_mut, handle_ref, CBox};
use crate::traits::{CCell, CDrop, CDupClone, CGuarded, CRefClone};

// ---------------------------------------------------------------------------
// CArc<T, D> — a shared reference to a refcounted C object
// ---------------------------------------------------------------------------

/// One counted reference to a refcounted C object, released on drop by the
/// policy's down-ref and cloned through its up_ref ([`CRefClone`]) — an
/// `Arc` whose count lives in the C object.
///
/// Every clone points at the same object, so a `CArc` hands out only the
/// shared handle ([`as_ref`](Self::as_ref)). Exclusive access needs the
/// refcount to prove this reference sole ([`get_mut`](Self::get_mut)), or a
/// private copy ([`make_mut`](Self::make_mut)). An object that is also
/// mutated after it is shared belongs in a [`CGuardedArc`].
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
        // SAFETY: we hold a live reference, and no clone of a `CArc` writes,
        // so the copy reads a quiescent object.
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
// CGuardedArc<T, D> — a shared reference reached through the object's lock
// ---------------------------------------------------------------------------

/// A [`CArc`] whose object is reached only through its own lock
/// ([`CGuarded`]): [`read`](Self::read) takes the read lock and returns a
/// [`CReadGuard`], [`write`](Self::write) takes the write lock and returns a
/// [`CWriteGuard`] — `Arc<RwLock<T>>` in one type, because the lock lives
/// inside the C object, behind the same pointer as the count. Each guard unlocks on drop, and the handles it hands out
/// cannot outlive it.
///
/// `write` takes `&self`: the lock, not the borrow, makes the access
/// exclusive, so any clone on any thread may write in turn. Readers proceed in
/// parallel when the C type has a reader/writer lock.
///
/// Locking panics if the C lock call reports failure, as `std`'s locks do on
/// an OS error; there is no poisoning, as in `parking_lot`. A second `write`
/// on a thread already holding a guard deadlocks, as `RwLock` may — or panics,
/// when the C lock reports the self-deadlock (`EDEADLK`) and is bound with
/// [`impl_cguarded!`](crate::impl_cguarded)'s `ok` check.
///
/// `Send` / `Sync` on `Arc<RwLock<T>>`'s terms: `T: Send + Sync`.
///
/// **Invariant in `T`**, as `Arc<RwLock<T>>` is. Clones share the object and
/// any of them may write, so a clone whose `T` shrank — `Holder<'static>` to
/// `Holder<'short>` — could store a `'short` borrow that the `'static`
/// original later reads:
///
/// ```compile_fail
/// # use core::marker::PhantomData;
/// # use core::ptr::NonNull;
/// # use ffibox::{CBorrowedPtr, CCell, CDrop, CGuarded, CGuardedArc, CRefClone, CWriteGuard};
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
/// #     unsafe fn c_lock(_: NonNull<Self>) -> bool { true }
/// #     unsafe fn c_unlock(_: NonNull<Self>) {}
/// # }
/// # #[derive(Clone)] pub struct Unref;
/// # unsafe impl<'x> CDrop<Holder<'x>> for Unref { unsafe fn c_drop(&self, _: NonNull<Holder<'x>>) {} }
/// # unsafe impl<'x> CRefClone<Holder<'x>> for Unref {
/// #     unsafe fn c_up_ref(&self, _: NonNull<Holder<'x>>) -> bool { true }
/// # }
/// fn shrink<'s>(a: CGuardedArc<Holder<'static>, Unref>) -> CGuardedArc<Holder<'s>, Unref> {
///     a
/// }
/// ```
#[repr(transparent)]
#[must_use = "dropping a CGuardedArc releases its reference"]
pub struct CGuardedArc<T: CGuarded, D: CDrop<T>>(CArc<T, D>, PhantomData<fn(T) -> T>);

impl<T: CGuarded, D: CDrop<T>> CGuardedArc<T, D> {
    #[inline]
    fn wrap(arc: CArc<T, D>) -> Self {
        Self(arc, PhantomData)
    }

    /// Adopt one counted reference under `D::default()`; `None` if null.
    ///
    /// # Safety
    ///
    /// As [`CArc::from_raw`].
    #[inline]
    pub unsafe fn from_raw(ptr: *mut T) -> Option<Self>
    where
        D: Default,
    {
        // SAFETY: the caller upholds the contract.
        unsafe { CArc::from_raw(ptr) }.map(Self::wrap)
    }

    /// Adopt one counted reference under `policy`; `None` if null.
    ///
    /// # Safety
    ///
    /// As [`CArc::from_raw_with`].
    #[inline]
    pub unsafe fn from_raw_with(ptr: *mut T, policy: D) -> Option<Self> {
        // SAFETY: the caller upholds the contract.
        unsafe { CArc::from_raw_with(ptr, policy) }.map(Self::wrap)
    }

    /// [`from_raw`](Self::from_raw) from the C type's pointer.
    ///
    /// # Safety
    ///
    /// As [`CArc::from_raw`].
    #[inline]
    pub unsafe fn from_c(ptr: *mut T::C) -> Option<Self>
    where
        D: Default,
    {
        // SAFETY: the caller upholds the contract.
        unsafe { CArc::from_c(ptr) }.map(Self::wrap)
    }

    /// [`from_raw_with`](Self::from_raw_with) from the C type's pointer.
    ///
    /// # Safety
    ///
    /// As [`CArc::from_raw_with`].
    #[inline]
    pub unsafe fn from_c_with(ptr: *mut T::C, policy: D) -> Option<Self> {
        // SAFETY: the caller upholds the contract.
        unsafe { CArc::from_c_with(ptr, policy) }.map(Self::wrap)
    }

    /// Raw pointer for passing to C. Calls that touch the object's state take
    /// its lock themselves, per [`CGuarded`]'s contract.
    #[inline]
    #[must_use]
    pub fn as_ptr(&self) -> *mut T {
        self.0.as_ptr()
    }

    /// [`as_ptr`](Self::as_ptr) as the C type's pointer.
    #[inline]
    #[must_use]
    pub fn as_c_ptr(&self) -> *mut T::C {
        self.0.as_c_ptr()
    }

    /// Whether two arcs reference the same object, like
    /// [`Arc::ptr_eq`](https://doc.rust-lang.org/std/sync/struct.Arc.html#method.ptr_eq).
    #[inline]
    #[must_use]
    pub fn ptr_eq(this: &Self, other: &Self) -> bool {
        CArc::ptr_eq(&this.0, &other.0)
    }

    /// The policy that will release the reference.
    #[inline]
    #[must_use]
    pub fn policy(&self) -> &D {
        self.0.policy()
    }

    /// Give up the reference without releasing it.
    #[inline]
    #[must_use = "the returned pointer owns a reference and must be released"]
    pub fn into_raw(self) -> *mut T {
        self.0.into_raw()
    }

    /// As [`into_raw`](Self::into_raw), also returning the policy.
    #[inline]
    #[must_use = "the returned pointer owns a reference and must be released"]
    pub fn into_raw_with(self) -> (*mut T, D) {
        self.0.into_raw_with()
    }

    /// [`into_raw`](Self::into_raw) as the C type's pointer.
    #[inline]
    #[must_use = "the returned pointer owns a reference and must be released"]
    pub fn into_c(self) -> *mut T::C {
        self.0.into_c()
    }

    /// Take the read lock: a guard handing out the shared handle, unlocking
    /// on drop. Like [`RwLock::read`](https://doc.rust-lang.org/std/sync/struct.RwLock.html#method.read).
    ///
    /// # Panics
    ///
    /// If the C lock call reports failure.
    #[inline]
    pub fn read(&self) -> CReadGuard<'_, T> {
        // SAFETY: our reference keeps the object live for the guard's borrow,
        // and every path to it locks.
        unsafe { read_lock(self.0.ptr) }
    }

    /// Take the write lock: a guard handing out the exclusive handle,
    /// unlocking on drop. Like [`RwLock::write`](https://doc.rust-lang.org/std/sync/struct.RwLock.html#method.write),
    /// it takes `&self`: the lock, not the borrow, makes the access exclusive.
    ///
    /// # Panics
    ///
    /// If the C lock call reports failure.
    #[inline]
    pub fn write(&self) -> CWriteGuard<'_, T> {
        // SAFETY: as `read`.
        unsafe { write_lock(self.0.ptr) }
    }
}

impl<T: CGuarded, D: CRefClone<T>> CGuardedArc<T, D> {
    /// Exclusive handle without locking, if this is provably the only
    /// reference; `None` otherwise.
    #[inline]
    #[must_use]
    pub fn get_mut(&mut self) -> Option<T::Mut<'_>> {
        self.0.get_mut()
    }

    /// Exclusive handle without locking, copying the object first (under its
    /// read lock) unless this is provably the only reference. `None` if the
    /// copy failed.
    #[inline]
    pub fn try_make_mut(&mut self) -> Option<T::Mut<'_>>
    where
        D: CDupClone<T>,
    {
        let ptr = self.0.ptr;
        // SAFETY: our reference keeps the object live.
        if !unsafe { T::c_read_lock(ptr) } {
            lock_failed();
        }
        // SAFETY: the read lock keeps writers on other clones out while the
        // copy reads the object.
        let copy = unsafe { copy_unless_sole(ptr, &self.0.policy) };
        // SAFETY: we took the read lock above, on this thread, and still hold
        // our reference, so the object is live.
        unsafe { T::c_read_unlock(ptr) };
        // Only now, unlocked, may our reference to the original be released:
        // it may be the last one, and the lock lives inside the object.
        // SAFETY: `copy` came from `copy_unless_sole` on our reference.
        unsafe { adopt_copy(&mut self.0.ptr, &self.0.policy, copy?) };
        // SAFETY: `self.0.ptr` is now the sole reference, exclusively borrowed.
        Some(unsafe { handle_mut(self.0.ptr) })
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
        // SAFETY: `try_make_mut` left `self.0.ptr` the sole reference.
        unsafe { handle_mut(self.0.ptr) }
    }

    /// Another reference through [`CRefClone::c_up_ref`]; `None` if it failed.
    #[inline]
    pub fn try_clone(&self) -> Option<Self>
    where
        D: Clone,
    {
        self.0.try_clone().map(Self::wrap)
    }

    /// The sole owner, if this is provably the only reference; `self` back
    /// otherwise.
    #[inline]
    pub fn try_into_box(self) -> Result<CBox<T, D>, Self> {
        self.0.try_into_box().map_err(Self::wrap)
    }
}

impl<T: CGuarded, D: CRefClone<T> + Clone> Clone for CGuardedArc<T, D> {
    #[inline]
    fn clone(&self) -> Self {
        Self::wrap(self.0.clone())
    }
}

impl<T: CGuarded, D: CDrop<T>> From<CBox<T, D>> for CGuardedArc<T, D> {
    /// Share a sole owner: its one reference becomes the first arc.
    #[inline]
    fn from(b: CBox<T, D>) -> Self {
        Self::wrap(CArc::from(b))
    }
}

impl<T: CGuarded, D: CRefClone<T>> TryFrom<CGuardedArc<T, D>> for CBox<T, D> {
    type Error = CGuardedArc<T, D>;

    /// [`CGuardedArc::try_into_box`].
    #[inline]
    fn try_from(arc: CGuardedArc<T, D>) -> Result<Self, Self::Error> {
        arc.try_into_box()
    }
}

impl<T: CGuarded, D: CDrop<T>> fmt::Debug for CGuardedArc<T, D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("CGuardedArc")
            .field(&self.0.as_ptr())
            .finish()
    }
}

impl<T: CGuarded, D: CDrop<T>> fmt::Pointer for CGuardedArc<T, D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Pointer::fmt(&self.0.as_ptr(), f)
    }
}

// `Send` / `Sync` come from the inner `CArc`: `T: Send + Sync`, as for
// `Arc<RwLock<T>>` — readers on several threads share `&T` access at once.

// ---------------------------------------------------------------------------
// CGuardedRef<'a, T> — a borrow reached through the object's lock
// ---------------------------------------------------------------------------

/// A borrow of an object that is reached only through its own lock
/// ([`CGuarded`]) — `&'a RwLock<T>`, where [`CGuardedArc`] is
/// `Arc<RwLock<T>>`. [`read`](Self::read) and [`write`](Self::write) return the
/// same [`CReadGuard`] / [`CWriteGuard`].
///
/// For an object nothing in Rust owns or releases: a C **global** guarded by a
/// C lock, borrowed for `'static`, or an object a parent keeps alive. A global
/// is never freed, so it needs no owner, only the lock:
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
/// registry().write().as_mut().add(42);
/// ```
///
/// `Copy`, like `&RwLock<T>`, and `Send` / `Sync` on its terms:
/// `T: Send + Sync`. **Invariant in `T`**, as `&RwLock<T>` is, since a copy
/// may write:
///
/// ```compile_fail
/// # use core::marker::PhantomData;
/// # use core::ptr::NonNull;
/// # use ffibox::{CBorrowedPtr, CCell, CGuarded, CGuardedRef};
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
/// #     unsafe fn c_lock(_: NonNull<Self>) -> bool { true }
/// #     unsafe fn c_unlock(_: NonNull<Self>) {}
/// # }
/// // `'a`, not `'static`: a `'static` borrow would force `'s` to be `'static`
/// // too, and the shrink would be a no-op.
/// fn shrink<'a, 's>(r: CGuardedRef<'a, Holder<'static>>) -> CGuardedRef<'a, Holder<'s>> {
///     r
/// }
/// ```
pub struct CGuardedRef<'a, T: CGuarded> {
    ptr: NonNull<T>,
    // Invariant in `T`, as a writer's borrow must be; `NonNull` withholds
    // `Send` / `Sync` until the impls below grant them on `&RwLock<T>`'s terms.
    _borrow: PhantomData<&'a mut T>,
}

impl<T: CGuarded> Clone for CGuardedRef<'_, T> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}
// Copies like `&RwLock<T>`; `#[derive]` would add a spurious `T: Copy`.
impl<T: CGuarded> Copy for CGuardedRef<'_, T> {}

impl<'a, T: CGuarded> CGuardedRef<'a, T> {
    /// Borrow the object at a C pointer; `None` if null. Borrows, so it is
    /// `from_ptr`, not `from_raw`: nothing is released when the view goes.
    ///
    /// # Safety
    ///
    /// - `ptr` must be null or address a live `T::C` that outlives `'a`.
    /// - [`CGuarded`]'s contract must hold for this object: every access, C's
    ///   included, takes its lock.
    /// - For `'a`, nothing may reach the object's handles without the lock —
    ///   no owner's lock-free `get_mut`, no handle built from the raw pointer.
    #[inline]
    pub unsafe fn from_ptr(ptr: *mut T::C) -> Option<Self> {
        NonNull::new(ptr.cast::<T>()).map(|ptr| Self {
            ptr,
            _borrow: PhantomData,
        })
    }

    /// Raw pointer for passing to C. Calls that touch the object's state take
    /// its lock themselves, per [`CGuarded`]'s contract.
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

    /// Take the read lock: a guard handing out the shared handle, unlocking
    /// on drop. By value, like a method on `&'a RwLock<T>`, so the guard keeps
    /// `'a` and `let g = registry().read();` outlives its statement.
    ///
    /// # Panics
    ///
    /// If the C lock call reports failure.
    #[inline]
    pub fn read(self) -> CReadGuard<'a, T> {
        // SAFETY: the object outlives `'a`, and every path to it locks, per
        // `from_ptr`.
        unsafe { read_lock(self.ptr) }
    }

    /// Take the write lock: a guard handing out the exclusive handle,
    /// unlocking on drop. As [`read`](Self::read); any copy on any thread may
    /// write in turn, because the lock, not the borrow, makes it exclusive.
    ///
    /// # Panics
    ///
    /// If the C lock call reports failure.
    #[inline]
    pub fn write(self) -> CWriteGuard<'a, T> {
        // SAFETY: as `read`.
        unsafe { write_lock(self.ptr) }
    }
}

impl<T: CGuarded> fmt::Debug for CGuardedRef<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("CGuardedRef")
            .field(&self.ptr.as_ptr())
            .finish()
    }
}

impl<T: CGuarded> fmt::Pointer for CGuardedRef<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Pointer::fmt(&self.ptr.as_ptr(), f)
    }
}

// SAFETY: as `&RwLock<T>`: a copy on another thread reads under the read lock
// (`T: Sync`) and writes under the write lock (`T: Send`).
unsafe impl<T: CGuarded + Send + Sync> Send for CGuardedRef<'_, T> {}
// SAFETY: as above; `&CGuardedRef` only copies the view out.
unsafe impl<T: CGuarded + Send + Sync> Sync for CGuardedRef<'_, T> {}

// ---------------------------------------------------------------------------
// The guards
// ---------------------------------------------------------------------------

/// A read lock on a [`CGuardedArc`]'s object, released on drop. Hands out the
/// shared handle, bound to the guard so it cannot outlive the lock.
///
/// `!Send`, like `RwLockReadGuard`: the unlock must run on the locking thread.
#[must_use = "dropping the guard releases the lock at once"]
pub struct CReadGuard<'a, T: CGuarded> {
    ptr: NonNull<T>,
    _borrow: PhantomData<(&'a T, *const ())>,
}

impl<T: CGuarded> CReadGuard<'_, T> {
    /// Shared handle to the locked object — the getters.
    #[inline]
    #[must_use]
    pub fn as_ref(&self) -> T::Ref<'_> {
        // SAFETY: the read lock excludes writers, and the handle is bound to
        // the guard's borrow, so it dies before the unlock.
        unsafe { handle_ref(self.ptr) }
    }
}

impl<T: CGuarded> Drop for CReadGuard<'_, T> {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: this guard took the read lock, on this thread (`!Send`).
        unsafe { T::c_read_unlock(self.ptr) }
    }
}

impl<T: CGuarded> fmt::Debug for CReadGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("CReadGuard")
            .field(&self.ptr.as_ptr())
            .finish()
    }
}

// SAFETY: sharing `&CReadGuard` shares `T::Ref` access, which `T: Sync`
// permits.
unsafe impl<T: CGuarded + Sync> Sync for CReadGuard<'_, T> {}

/// The write lock on a [`CGuardedArc`]'s object, released on drop. Hands out
/// the exclusive handle, bound to the guard so it cannot outlive the lock:
///
/// ```compile_fail,E0505
/// # use core::ptr::NonNull;
/// # use ffibox::{define_ctype, CDrop, CGuarded, CGuardedArc};
/// # #[repr(C)] pub struct obj_st { x: u8 }
/// # define_ctype!(Obj, ObjRef, ObjMut, obj_st);
/// # unsafe impl CGuarded for Obj {
/// #     unsafe fn c_lock(_: NonNull<Self>) -> bool { true }
/// #     unsafe fn c_unlock(_: NonNull<Self>) {}
/// # }
/// # #[derive(Default)] struct Unref;
/// # unsafe impl CDrop<Obj> for Unref { unsafe fn c_drop(&self, _: NonNull<Obj>) {} }
/// fn escape(arc: &CGuardedArc<Obj, Unref>) {
///     let mut guard = arc.write();
///     let handle = guard.as_mut();
///     drop(guard); // unlocks here ...
///     drop(handle); // ... so the handle may not live on
/// }
/// ```
///
/// `!Send`, like `MutexGuard`: the unlock must run on the locking thread.
///
/// **Invariant in `T`**, as `RwLockWriteGuard` is, since it hands out the
/// exclusive handle: a guard over `Holder<'static>` must not become one over
/// `Holder<'short>`, which could store a `'short` borrow in the object.
///
/// ```compile_fail
/// # use core::marker::PhantomData;
/// # use core::ptr::NonNull;
/// # use ffibox::{CBorrowedPtr, CCell, CDrop, CGuarded, CGuardedArc, CRefClone, CWriteGuard};
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
/// #     unsafe fn c_lock(_: NonNull<Self>) -> bool { true }
/// #     unsafe fn c_unlock(_: NonNull<Self>) {}
/// # }
/// # #[derive(Clone)] pub struct Unref;
/// # unsafe impl<'x> CDrop<Holder<'x>> for Unref { unsafe fn c_drop(&self, _: NonNull<Holder<'x>>) {} }
/// # unsafe impl<'x> CRefClone<Holder<'x>> for Unref {
/// #     unsafe fn c_up_ref(&self, _: NonNull<Holder<'x>>) -> bool { true }
/// # }
/// fn shrink<'g, 's>(g: CWriteGuard<'g, Holder<'static>>) -> CWriteGuard<'g, Holder<'s>> {
///     g
/// }
/// ```
#[must_use = "dropping the guard releases the lock at once"]
pub struct CWriteGuard<'a, T: CGuarded> {
    ptr: NonNull<T>,
    _borrow: PhantomData<(&'a mut T, *const ())>,
}

impl<T: CGuarded> CWriteGuard<'_, T> {
    /// Shared handle to the locked object — the getters.
    #[inline]
    #[must_use]
    pub fn as_ref(&self) -> T::Ref<'_> {
        // SAFETY: the write lock excludes every other guard; the handle is
        // bound to the guard's borrow.
        unsafe { handle_ref(self.ptr) }
    }

    /// Exclusive handle to the locked object — the setters.
    #[inline]
    #[must_use]
    pub fn as_mut(&mut self) -> T::Mut<'_> {
        // SAFETY: as `as_ref`, from an exclusive borrow of the guard, so this
        // is the only handle in use.
        unsafe { handle_mut(self.ptr) }
    }
}

impl<T: CGuarded> Drop for CWriteGuard<'_, T> {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: this guard took the write lock, on this thread (`!Send`).
        unsafe { T::c_unlock(self.ptr) }
    }
}

impl<T: CGuarded> fmt::Debug for CWriteGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("CWriteGuard")
            .field(&self.ptr.as_ptr())
            .finish()
    }
}

// SAFETY: sharing `&CWriteGuard` shares only `T::Ref` access, which `T: Sync`
// permits.
unsafe impl<T: CGuarded + Sync> Sync for CWriteGuard<'_, T> {}

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

/// Take `ptr`'s read lock and wrap it in a guard; panics if the lock fails.
///
/// # Safety
///
/// `ptr` must address a live `T` that outlives `'a`, reached by every path
/// under its lock.
#[inline]
unsafe fn read_lock<'a, T: CGuarded>(ptr: NonNull<T>) -> CReadGuard<'a, T> {
    // SAFETY: the caller keeps `ptr` live for `'a`.
    if !unsafe { T::c_read_lock(ptr) } {
        lock_failed();
    }
    CReadGuard {
        ptr,
        _borrow: PhantomData,
    }
}

/// Take `ptr`'s write lock and wrap it in a guard; panics if the lock fails.
///
/// # Safety
///
/// As [`read_lock`].
#[inline]
unsafe fn write_lock<'a, T: CGuarded>(ptr: NonNull<T>) -> CWriteGuard<'a, T> {
    // SAFETY: as `read_lock`.
    if !unsafe { T::c_lock(ptr) } {
        lock_failed();
    }
    CWriteGuard {
        ptr,
        _borrow: PhantomData,
    }
}

/// A C lock call reported failure. Panicking is safe here: no guard exists
/// yet, so nothing is left locked.
#[cold]
#[inline(never)]
#[track_caller]
fn lock_failed() -> ! {
    panic!("ffibox: the C object's lock call failed")
}
