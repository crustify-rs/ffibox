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
//! that every borrow of the old value has ended (is no longer live, not only
//! unused), and that no weak pointer C keeps to it is used afterwards.

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
        unsafe { NonNull::new_unchecked(crate::counted::CShared::into_raw(self)) }
    }
    #[inline]
    unsafe fn from_raw_ptr(ptr: NonNull<T>) -> Self {
        // SAFETY: per this function's contract.
        unsafe { Self::from_raw(ptr.as_ptr()).unwrap_unchecked() }
    }
}

/// A [`CPtrOwner`] whose shared access is its pointee: what the field's
/// `*_target` methods lend, read through the pointer the field holds
/// (loaded atomically) rather than through a reference to the field.
///
/// Implemented for `Box<T>` (`View = T`), [`CShared<T>`](crate::CShared)
/// when `T` is [`CShareable`](crate::CShareable) (`View = T`, what it
/// derefs to) and [`CStrBox`] (`View = CStr`). Not for [`CBox`] or
/// [`CArc`], whose objects are reached through handles, never through a
/// reference:
///
/// ```compile_fail,E0599
/// # use ffibox::{CAtomicOwner, CBox, define_ctype, impl_cdrop};
/// # #[repr(C)] pub struct x_st { v: u8 }
/// # define_ctype!(X, XRef, XMut, x_st);
/// # #[derive(Clone, Copy, Debug, Default)] pub struct XFree;
/// # unsafe fn x_free(_: *mut x_st) {}
/// # impl_cdrop!(XFree, X, x_free);
/// let f = CAtomicOwner::<CBox<X, XFree>>::new();
/// let _ = f.get_target();
/// ```
///
/// # Safety
///
/// For a pointer `ptr` that [`into_raw_ptr`](CPtrOwner::into_raw_ptr)
/// returned, of an owner that stays held (not adopted back, not dropped)
/// and is only shared-borrowed for `'a`: [`view`](Self::view)`(ptr)` is
/// valid for `'a`, is what the owner's own shared access (its `Deref`)
/// gives, and reads only memory the owner points to, never the place the
/// pointer is stored in.
pub unsafe trait CPtrDeref: CPtrOwner {
    /// What a shared borrow of the owner gives.
    type View: ?Sized;

    /// The view of the owner whose pointer is `ptr`.
    ///
    /// # Safety
    ///
    /// `ptr` came from [`into_raw_ptr`](CPtrOwner::into_raw_ptr), and that
    /// owner stays held and is not written (only shared-borrowed) for `'a`.
    unsafe fn view<'a>(ptr: NonNull<Self::Target>) -> &'a Self::View;
}

// SAFETY: a held `Box<T>` owns a live `T` at its pointer, which its `Deref`
// gives; while the box is only shared-borrowed nothing writes it.
#[cfg(feature = "alloc")]
unsafe impl<T> CPtrDeref for alloc::boxed::Box<T> {
    type View = T;
    #[inline]
    unsafe fn view<'a>(ptr: NonNull<T>) -> &'a T {
        // SAFETY: the held box's `T` (this fn's contract), live and not
        // written for `'a`.
        unsafe { ptr.as_ref() }
    }
}

// SAFETY: a held `CShared<T>` keeps the object live, and `CShareable`
// makes a `&T` sound while it is shared (its `Deref` gives exactly that).
#[cfg(feature = "alloc")]
unsafe impl<T: crate::counted::CShareable> CPtrDeref for crate::counted::CShared<T> {
    type View = T;
    #[inline]
    unsafe fn view<'a>(ptr: NonNull<T>) -> &'a T {
        // SAFETY: a live object, kept so by the held owner for `'a` (this
        // fn's contract), which a shared reference may cover while it is
        // shared (`CShareable`).
        unsafe { ptr.as_ref() }
    }
}

// SAFETY: a held `CStrBox` owns a live, NUL-terminated string that nothing
// writes (the type is read-only), which its `Deref` gives as a `CStr`.
unsafe impl<D: CDrop<core::ffi::c_char> + Default> CPtrDeref for CStrBox<D> {
    type View = core::ffi::CStr;
    #[inline]
    unsafe fn view<'a>(ptr: NonNull<core::ffi::c_char>) -> &'a core::ffi::CStr {
        // SAFETY: the held box's string (this fn's contract): live and
        // NUL-terminated, unwritten for `'a`.
        unsafe { core::ffi::CStr::from_ptr(ptr.as_ptr()) }
    }
}

/// An owner `O` held in a pointer field that C reads, and fills with
/// compare-and-swap: `AtomicPtr<O::Target>` holding an `O`, with
/// `OnceLock<O>`'s methods for filling it once.
///
/// | Method | Counterpart | Does |
/// |---|---|---|
/// | [`get`](Self::get) | `OnceLock::get` | borrows the value, `None` while empty |
/// | [`try_insert_ptr`](Self::try_insert_ptr) | — | fills an empty field, lending nothing: the pointers, as [`as_ptr`](Self::as_ptr) |
/// | [`get_target`](Self::get_target), [`try_insert_target`](Self::try_insert_target), [`get_or_init_target`](Self::get_or_init_target), [`get_or_try_init_target`](Self::get_or_try_init_target) | — | the same, lending the owner's pointee (`O: CPtrDeref`) instead of the owner |
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
/// **Fields shared between threads: use the `*_target` methods.** [`get`],
/// [`try_insert`], [`get_or_init`] and [`get_or_try_init`] lend `&O`, a
/// reference to the field's own storage: reading through it (an owner's
/// `Deref` reads its pointer) reads the `AtomicPtr`'s bytes non-atomically,
/// while another thread may run a compare-and-swap on them in
/// [`try_insert`]. On a full field that compare-and-swap fails, and a
/// failed one is a load in the Rust (C++20) memory model, so this is not a
/// data race there; but ThreadSanitizer models every compare-and-swap as a
/// write and reports it, and the code gains nothing from the reference. The
/// `*_target` methods (for an `O: CPtrDeref`) load the pointer atomically
/// and lend what it points to, so no reference to the field is ever formed
/// while other threads may write it; for other owners, [`as_ptr`] and
/// [`try_insert_ptr`] give the pointer.
///
/// [`as_ptr`]: Self::as_ptr
/// [`try_insert_ptr`]: Self::try_insert_ptr
///
/// [`get`]: Self::get
/// [`try_insert`]: Self::try_insert
/// [`get_or_init`]: Self::get_or_init
/// [`get_or_try_init`]: Self::get_or_try_init
///
/// All-zero bytes are an empty field, as a NULL pointer is: a struct that
/// C or Rust `calloc`s starts with its `CAtomicOwner` fields empty.
///
/// **What C may do.** C reads the field, and fills it when it is NULL, with
/// an owner of the same kind. It replaces or clears a full field only where
/// the binding's contract with C already says every borrow of the old value
/// has ended (is no longer live), as [`replace`](Self::replace) states for
/// Rust. The field
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
/// nor exclusively borrowed:
///
/// ```compile_fail,E0502
/// use ffibox::CAtomicOwner;
/// let mut slot = CAtomicOwner::<Box<u32>>::new();
/// let v = slot.get_or_init(|| Box::new(1));
/// let m = slot.get_mut();
/// assert_eq!(**v, 1);
/// # drop(m);
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

    /// [`try_insert`](Self::try_insert), lending nothing: `Ok` with the
    /// inserted owner's pointer, or `Err` with the held one's and `value`
    /// back. For owners whose object is reached through a handle or a
    /// pointer (and which are not [`CPtrDeref`]); the pointer is valid
    /// while the field holds its owner, as [`as_ptr`](Self::as_ptr)'s is.
    #[allow(clippy::type_complexity)] // `try_insert`'s shape, with pointers
    pub fn try_insert_ptr(&self, value: O) -> Result<NonNull<O::Target>, (NonNull<O::Target>, O)> {
        let new = value.into_raw_ptr();
        match self.ptr.compare_exchange(
            ptr::null_mut(),
            new.as_ptr(),
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => Ok(new),
            Err(cur) => {
                // SAFETY: `new` was not published: the owner is still ours.
                let value = unsafe { O::from_raw_ptr(new) };
                // SAFETY: the compare-and-swap failed on a full field, so
                // `cur` is its non-null value.
                Err((unsafe { NonNull::new_unchecked(cur) }, value))
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
    /// Every borrow of the old value that [`get`](Self::get) and the other
    /// borrowing methods lent has ended, on every thread: not only unused
    /// from now on, but no longer live (a reference held in a caller's
    /// argument is live until that call returns, and the write would
    /// invalidate it). No pointer C read from the field and keeps as a weak
    /// pointer is used afterwards.
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

impl<O: CPtrDeref> CAtomicOwner<O> {
    /// The pointee of the owner at the non-null `p`, borrowed for `&self`.
    ///
    /// # Safety
    ///
    /// As [`lend`](Self::lend).
    #[inline]
    unsafe fn lend_target(&self, p: *mut O::Target) -> &O::View {
        // SAFETY: `p` is the field's current value (the contract): the
        // pointer of an owner the field holds, which stays held and is
        // only shared-borrowed while `&self` lives (only `replace`, whose
        // caller ends every borrow first, and `&mut self` methods take it
        // out). The view reads the pointee, not the field.
        unsafe { O::view(NonNull::new_unchecked(p)) }
    }

    /// The owner's pointee (what its `Deref` gives), borrowed while `self`
    /// is; `None` while the field is empty. Unlike [`get`](Self::get), it
    /// forms no reference to the field: the pointer is loaded atomically
    /// (see the type's documentation).
    #[inline]
    pub fn get_target(&self) -> Option<&O::View> {
        let p = self.ptr.load(Ordering::Acquire);
        if p.is_null() {
            None
        } else {
            // SAFETY: the field's value, read with `Acquire`.
            Some(unsafe { self.lend_target(p) })
        }
    }

    /// [`try_insert`](Self::try_insert), lending the pointee: `Ok` with
    /// the inserted value's, or `Err` with the held value's and `value`
    /// back.
    pub fn try_insert_target(&self, value: O) -> Result<&O::View, (&O::View, O)> {
        let new = value.into_raw_ptr();
        match self.ptr.compare_exchange(
            ptr::null_mut(),
            new.as_ptr(),
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            // SAFETY: this thread wrote `new` into the field.
            Ok(_) => Ok(unsafe { self.lend_target(new.as_ptr()) }),
            Err(cur) => {
                // SAFETY: `new` was not published: the owner is still ours.
                let value = unsafe { O::from_raw_ptr(new) };
                // SAFETY: the full field's value, read with `Acquire`.
                Err((unsafe { self.lend_target(cur) }, value))
            }
        }
    }

    /// [`get_or_init`](Self::get_or_init), lending the pointee.
    #[inline]
    pub fn get_or_init_target(&self, f: impl FnOnce() -> O) -> &O::View {
        match self.get_or_try_init_target(|| Ok::<O, core::convert::Infallible>(f())) {
            Ok(v) => v,
            Err(e) => match e {},
        }
    }

    /// [`get_or_try_init`](Self::get_or_try_init), lending the pointee.
    pub fn get_or_try_init_target<E>(
        &self,
        f: impl FnOnce() -> Result<O, E>,
    ) -> Result<&O::View, E> {
        if let Some(v) = self.get_target() {
            return Ok(v);
        }
        match self.try_insert_target(f()?) {
            Ok(v) => Ok(v),
            Err((winner, loser)) => {
                drop(loser);
                Ok(winner)
            }
        }
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
