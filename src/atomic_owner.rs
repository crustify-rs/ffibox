//! [`CAtomicOwner<O>`]: an owner held in a pointer field that C reads and
//! sets with compare-and-swap: `AtomicPtr` and `OnceLock` for an owner that
//! is one pointer ([`CPtrOwner`]).
//!
//! A C struct often keeps a lazily created object behind a pointer field:
//! the first caller to need it creates one and publishes it with
//! `compare_and_swap(&obj->field, NULL, new)`; a caller that loses the race
//! frees its own and uses the winner's. Callers borrow the published object
//! for as long as the struct holds it (C's "weak pointer"), and a setter
//! replaces it, whose callers promise that no borrow outlives the change.
//!
//! `CAtomicOwner<O>` is that field: `#[repr(transparent)]` over
//! `AtomicPtr<O::Target>`, so it has the layout of the C pointer, holding
//! the owner `O` (a [`CBox`], [`CArc`],
//! [`CStrBox`], [`CShared`](crate::CShared) or `Box`). Its
//! loser releases itself, by the owner's drop:
//!
//! ```
//! use ffibox::CAtomicOwner;
//!
//! struct Repo {
//!     // `git_odb *_odb;` in C's struct.
//!     odb: CAtomicOwner<Box<String>>,
//! }
//!
//! let repo = Repo { odb: CAtomicOwner::new() };
//! assert!(repo.odb.get().is_none());
//! let odb = repo.odb.get_or_init(|| Box::new("objects".into()));
//! assert_eq!(**odb, "objects");
//! // Another thread that lost the race gets its own value back.
//! let (winner, loser) = repo.odb.try_insert(Box::new("other".into())).unwrap_err();
//! assert!(core::ptr::eq(winner, odb));
//! drop(loser);
//! // C's `repo->_odb`, a borrowed pointer.
//! assert_eq!(repo.odb.as_ptr(), &raw const **odb as *mut String);
//! ```
//!
//! Safe code only fills an empty field, so a borrow it lends stays valid
//! while `&self` lives. Replacing a value that may be lent is
//! [`replace`](CAtomicOwner::replace), which is `unsafe`: its caller states
//! that no borrow of the old value, Rust's or C's, is used afterwards.

use core::fmt;
use core::marker::PhantomData;
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicPtr, Ordering};

use crate::refs::{CBox, CStrBox};
use crate::shared::CArc;
use crate::traits::{CCell, CDrop};

/// An owner that is one non-null pointer: what a [`CAtomicOwner`] holds.
/// Its methods are `into_raw` and `from_raw` under names of their own, so
/// that they do not shadow the owner's inherent ones.
///
/// Implemented for the pointer owners: [`CBox`], [`CArc`], [`CStrBox`] (under
/// a policy that is `Default`), [`CShared`](crate::CShared) and `Box<T>`.
///
/// # Safety
///
/// - `Self` has the size, alignment and representation of
///   `NonNull<Self::Target>`, and its value is the pointer that
///   [`into_raw_ptr`](Self::into_raw_ptr) returns: a shared reference to
///   `Self` may be formed over a pointer `into_raw_ptr` returned, while the
///   owner is held there, and is that owner.
/// - [`from_raw_ptr`](Self::from_raw_ptr) of what `into_raw_ptr` returned
///   is the same owner again.
pub unsafe trait CPtrOwner: Sized {
    /// What the pointer addresses.
    type Target;

    /// Surrender the owner as its pointer.
    fn into_raw_ptr(self) -> NonNull<Self::Target>;

    /// Adopt an owner its pointer surrendered.
    ///
    /// # Safety
    ///
    /// `ptr` came from [`into_raw_ptr`](Self::into_raw_ptr), and its owner
    /// was not adopted since.
    unsafe fn from_raw_ptr(ptr: NonNull<Self::Target>) -> Self;
}

// SAFETY: `CBox` is `#[repr(transparent)]` over its `NonNull` with a
// stateless policy, which `from_raw` rebuilds as `D::default()`.
unsafe impl<T: CCell, D: CDrop<T> + Default> CPtrOwner for CBox<T, D> {
    type Target = T;
    #[inline]
    fn into_raw_ptr(self) -> NonNull<T> {
        // SAFETY: a box's pointer is non-null.
        unsafe { NonNull::new_unchecked(self.into_raw()) }
    }
    #[inline]
    unsafe fn from_raw_ptr(ptr: NonNull<T>) -> Self {
        // SAFETY: per this function's contract, the box `into_raw_ptr` gave
        // up.
        unsafe { Self::from_raw(ptr.as_ptr()).unwrap_unchecked() }
    }
}

// SAFETY: as for `CBox`: `CArc` is `#[repr(transparent)]` over its pointer.
unsafe impl<T: CCell, D: CDrop<T> + Default> CPtrOwner for CArc<T, D> {
    type Target = T;
    #[inline]
    fn into_raw_ptr(self) -> NonNull<T> {
        // SAFETY: an arc's pointer is non-null.
        unsafe { NonNull::new_unchecked(self.into_raw()) }
    }
    #[inline]
    unsafe fn from_raw_ptr(ptr: NonNull<T>) -> Self {
        // SAFETY: per this function's contract.
        unsafe { Self::from_raw(ptr.as_ptr()).unwrap_unchecked() }
    }
}

// SAFETY: as for `CBox`: `CStrBox` is `#[repr(transparent)]` over its pointer.
unsafe impl<D: CDrop<core::ffi::c_char> + Default> CPtrOwner for CStrBox<D> {
    type Target = core::ffi::c_char;
    #[inline]
    fn into_raw_ptr(self) -> NonNull<core::ffi::c_char> {
        // SAFETY: a string box's pointer is non-null.
        unsafe { NonNull::new_unchecked(self.into_raw()) }
    }
    #[inline]
    unsafe fn from_raw_ptr(ptr: NonNull<core::ffi::c_char>) -> Self {
        // SAFETY: per this function's contract.
        unsafe { Self::from_raw(ptr.as_ptr()).unwrap_unchecked() }
    }
}

// SAFETY: `Box<T>` of a sized `T` is guaranteed to be one non-null pointer,
// the one `Box::into_raw` returns.
#[cfg(feature = "alloc")]
unsafe impl<T> CPtrOwner for alloc::boxed::Box<T> {
    type Target = T;
    #[inline]
    fn into_raw_ptr(self) -> NonNull<T> {
        NonNull::from(alloc::boxed::Box::leak(self))
    }
    #[inline]
    unsafe fn from_raw_ptr(ptr: NonNull<T>) -> Self {
        // SAFETY: per this function's contract, a box `into_raw_ptr` leaked.
        unsafe { alloc::boxed::Box::from_raw(ptr.as_ptr()) }
    }
}

// SAFETY: `CShared` is `#[repr(transparent)]` over its `NonNull`.
#[cfg(feature = "alloc")]
unsafe impl<T: crate::counted::CCounted> CPtrOwner for crate::counted::CShared<T> {
    type Target = T;
    #[inline]
    fn into_raw_ptr(self) -> NonNull<T> {
        // SAFETY: a shared reference's pointer is non-null.
        unsafe { NonNull::new_unchecked(self.into_raw()) }
    }
    #[inline]
    unsafe fn from_raw_ptr(ptr: NonNull<T>) -> Self {
        // SAFETY: per this function's contract.
        unsafe { Self::from_raw(ptr.as_ptr()).unwrap_unchecked() }
    }
}

/// An owner `O` held in a pointer field that C reads, and fills with
/// compare-and-swap: `AtomicPtr<O::Target>` holding an `O`, with
/// `OnceLock<O>`'s methods for filling it once.
///
/// | Method | Counterpart | Does |
/// |---|---|---|
/// | [`get`](Self::get) | `OnceLock::get` | borrows the value, `None` while empty |
/// | [`try_insert`](Self::try_insert), [`set`](Self::set) | `OnceLock::try_insert`, `OnceLock::set` | fill an empty field; a full one hands the value back |
/// | [`get_or_init`](Self::get_or_init), [`get_or_try_init`](Self::get_or_try_init) | `OnceLock::get_or_init`, `get_or_try_init` | create on first use; the loser of a race is dropped |
/// | [`replace`](Self::replace) (`unsafe`) | `AtomicPtr::swap` | replace a value that may be lent |
/// | [`take`](Self::take), [`get_mut`](Self::get_mut), [`into_inner`](Self::into_inner) | `OnceLock::take`, `get_mut`, `into_inner` | exclusive access |
/// | [`as_ptr`](Self::as_ptr) | `AtomicPtr::load` | the pointer, for C's weak pointer |
///
/// Unlike `OnceLock`, initialisation does not block: two threads may each
/// create a value, and the one that loses the race drops its own, as C's
/// compare-and-swap does. Where creating a value has side effects that the
/// loser must undo first (an owner it set), use
/// [`try_insert`](Self::try_insert), which hands the loser back.
///
/// **What C may do.** C reads the field, and fills it when it is NULL, with
/// an owner of the same kind. It replaces or clears a full field only where
/// the binding's contract with C already says no borrow of the old value is
/// used afterwards, as [`replace`](Self::replace) states for Rust. The field
/// holds `NULL` or a pointer the owner's [`into_raw_ptr`](CPtrOwner) gave.
///
/// A borrow keeps the field from being emptied through `&mut self`:
///
/// ```compile_fail,E0502
/// use ffibox::CAtomicOwner;
/// let mut slot = CAtomicOwner::<Box<u32>>::new();
/// let v = slot.get_or_init(|| Box::new(1));
/// let old = slot.take();
/// assert_eq!(**v, 1);
/// ```
///
/// and replacing it through `&self` is `unsafe`:
///
/// ```compile_fail,E0133
/// use ffibox::CAtomicOwner;
/// let slot = CAtomicOwner::<Box<u32>>::new();
/// let v = slot.get_or_init(|| Box::new(1));
/// let old = slot.replace(Some(Box::new(2)));
/// ```
///
/// It holds an owner that is one pointer, and nothing else:
///
/// ```compile_fail,E0277
/// use ffibox::CAtomicOwner;
/// let slot = CAtomicOwner::<Vec<u8>>::new(); // a `Vec` is three words
/// ```
///
/// `Send` when `O` is, and `Sync` when `O` is `Send + Sync`, as `OnceLock`:
/// a value one thread inserts may be borrowed, and dropped, on another.
///
/// ```compile_fail,E0277
/// use ffibox::CAtomicOwner;
/// fn sync<T: Sync>(_: &T) {}
/// sync(&CAtomicOwner::<Box<core::cell::Cell<u32>>>::new());
/// ```
#[repr(transparent)]
pub struct CAtomicOwner<O: CPtrOwner> {
    ptr: AtomicPtr<O::Target>,
    _owns: PhantomData<O>,
    // `Send` and `Sync` are the impls' below, not `AtomicPtr`'s.
    _not_auto: PhantomData<*const ()>,
}

// SAFETY: moving the field moves the owner it holds (`O: Send`).
unsafe impl<O: CPtrOwner + Send> Send for CAtomicOwner<O> {}
// SAFETY: a shared field lends `&O` to every thread (`O: Sync`), and takes an
// `O` from one thread that another may drop (`O: Send`), as `OnceLock` does.
unsafe impl<O: CPtrOwner + Send + Sync> Sync for CAtomicOwner<O> {}

impl<O: CPtrOwner> CAtomicOwner<O> {
    /// The size check: the field is one C pointer.
    const ONE_POINTER: () = assert!(
        core::mem::size_of::<O>() == core::mem::size_of::<*mut O::Target>()
            && core::mem::align_of::<O>() == core::mem::align_of::<*mut O::Target>(),
        "ffibox: a CPtrOwner is one pointer"
    );

    /// An empty field, C's `NULL`.
    #[inline]
    #[must_use]
    pub const fn new() -> Self {
        Self {
            ptr: AtomicPtr::new(ptr::null_mut()),
            _owns: PhantomData,
            _not_auto: PhantomData,
        }
    }

    /// A field holding `value`.
    #[inline]
    #[must_use]
    pub fn with(value: O) -> Self {
        Self {
            ptr: AtomicPtr::new(value.into_raw_ptr().as_ptr()),
            _owns: PhantomData,
            _not_auto: PhantomData,
        }
    }

    /// The owner held at the non-null `p`, borrowed for `&self`.
    ///
    /// # Safety
    ///
    /// `p` is the field's current, non-null value, read with `Acquire` or
    /// written by this thread, and the field is not replaced while the
    /// borrow lives ([`replace`](Self::replace)'s contract).
    #[inline]
    unsafe fn lend(&self, p: *mut O::Target) -> &O {
        let () = Self::ONE_POINTER;
        debug_assert!(!p.is_null());
        // SAFETY: the field holds `p`, a pointer `into_raw_ptr` gave, and
        // nothing writes it while the borrow lives, so its bytes are that
        // owner (`CPtrOwner`'s contract) and a shared reference may cover
        // them; other threads only read them, atomically or through such a
        // reference.
        unsafe { &*self.ptr.as_ptr().cast::<O>() }
    }

    /// The value, borrowed while `self` is; `None` while the field is empty.
    #[inline]
    pub fn get(&self) -> Option<&O> {
        let p = self.ptr.load(Ordering::Acquire);
        if p.is_null() {
            None
        } else {
            // SAFETY: the field's value, read with `Acquire`; only `replace`
            // writes a full field, and its caller ends this borrow first.
            Some(unsafe { self.lend(p) })
        }
    }

    /// Fill the field if it is empty: `Ok` with the value, now borrowed, or,
    /// if it was full, `Err` with the value it holds and `value` back.
    pub fn try_insert(&self, value: O) -> Result<&O, (&O, O)> {
        let new = value.into_raw_ptr();
        match self.ptr.compare_exchange(
            ptr::null_mut(),
            new.as_ptr(),
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            // SAFETY: this thread wrote `new` into the field.
            Ok(_) => Ok(unsafe { self.lend(new.as_ptr()) }),
            Err(cur) => {
                // SAFETY: `new` was not published: the owner is still ours.
                let value = unsafe { O::from_raw_ptr(new) };
                // SAFETY: the full field's value, read with `Acquire`.
                Err((unsafe { self.lend(cur) }, value))
            }
        }
    }

    /// Fill the field if it is empty, or hand `value` back.
    #[inline]
    pub fn set(&self, value: O) -> Result<(), O> {
        self.try_insert(value).map(|_| ()).map_err(|(_, v)| v)
    }

    /// The value, created by `f` if the field is empty. If another thread
    /// fills the field meanwhile, the value `f` made is dropped and the
    /// other one returned.
    #[inline]
    pub fn get_or_init(&self, f: impl FnOnce() -> O) -> &O {
        match self.get_or_try_init(|| Ok::<O, core::convert::Infallible>(f())) {
            Ok(v) => v,
            Err(e) => match e {},
        }
    }

    /// As [`get_or_init`](Self::get_or_init), for an `f` that may fail; the
    /// field stays empty if it does.
    pub fn get_or_try_init<E>(&self, f: impl FnOnce() -> Result<O, E>) -> Result<&O, E> {
        if let Some(v) = self.get() {
            return Ok(v);
        }
        match self.try_insert(f()?) {
            Ok(v) => Ok(v),
            Err((winner, loser)) => {
                drop(loser);
                Ok(winner)
            }
        }
    }

    /// Replace the value, full or empty, returning the old one.
    ///
    /// # Safety
    ///
    /// No borrow of the old value is used after the call: none that
    /// [`get`](Self::get) and the other borrowing methods lent, on any
    /// thread, and no pointer C read from the field and keeps as a weak
    /// pointer.
    #[inline]
    pub unsafe fn replace(&self, value: Option<O>) -> Option<O> {
        let new = value.map_or(ptr::null_mut(), |v| v.into_raw_ptr().as_ptr());
        let old = self.ptr.swap(new, Ordering::AcqRel);
        // SAFETY: the field held NULL or an owner's pointer, which the swap
        // took out of it: the owner is ours.
        NonNull::new(old).map(|p| unsafe { O::from_raw_ptr(p) })
    }

    /// Empty the field, returning its value.
    #[inline]
    pub fn take(&mut self) -> Option<O> {
        let old = core::mem::replace(self.ptr.get_mut(), ptr::null_mut());
        // SAFETY: as in `replace`; `&mut self` excludes every borrow.
        NonNull::new(old).map(|p| unsafe { O::from_raw_ptr(p) })
    }

    /// The value, exclusively.
    #[inline]
    pub fn get_mut(&mut self) -> Option<&mut O> {
        let () = Self::ONE_POINTER;
        let slot = self.ptr.get_mut();
        if slot.is_null() {
            None
        } else {
            // SAFETY: a full field's bytes are its owner (`CPtrOwner`'s
            // contract), and `&mut self` excludes every other access. An `O`
            // written through the result is a non-null owner's pointer
            // again.
            Some(unsafe { &mut *ptr::from_mut(slot).cast::<O>() })
        }
    }

    /// The value, taking the field apart.
    #[inline]
    pub fn into_inner(mut self) -> Option<O> {
        self.take()
    }

    /// The field's pointer, which stays with the field: C's weak pointer, as
    /// `repo->_odb` is. NULL while the field is empty.
    #[inline]
    #[must_use]
    pub fn as_ptr(&self) -> *mut O::Target {
        self.ptr.load(Ordering::Acquire)
    }
}

impl<O: CPtrOwner> Default for CAtomicOwner<O> {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl<O: CPtrOwner> From<O> for CAtomicOwner<O> {
    #[inline]
    fn from(value: O) -> Self {
        Self::with(value)
    }
}

impl<O: CPtrOwner> From<Option<O>> for CAtomicOwner<O> {
    #[inline]
    fn from(value: Option<O>) -> Self {
        value.map_or_else(Self::new, Self::with)
    }
}

impl<O: CPtrOwner> Drop for CAtomicOwner<O> {
    #[inline]
    fn drop(&mut self) {
        drop(self.take());
    }
}

impl<O: CPtrOwner + fmt::Debug> fmt::Debug for CAtomicOwner<O> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("CAtomicOwner").field(&self.get()).finish()
    }
}
