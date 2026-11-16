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
//! [`CCounted`]: how a reference is taken and dropped, when the object is
//! released, and how it is freed. [`CShareable`] adds that a `&T` may be held
//! while C holds references too, which is what lets `CShared` deref.
//!
//! ```
//! use core::ptr::NonNull;
//! use core::sync::atomic::{AtomicI32, AtomicU32, Ordering};
//! use ffibox::counted::{CCounted, CShareable, CShared};
//!
//! /// `struct session { int rc; ... }`: C increments `rc` in place, and
//! /// calls `session_free`, below, to drop a reference.
//! #[repr(C)]
//! pub struct Session {
//!     rc: AtomicI32,
//!     /// Rust-private state after the C-visible prefix.
//!     hits: AtomicU32,
//! }
//!
//! // SAFETY: `rc` counts the references; it is atomic and is the only field
//! // C writes, as C's macros do, with the same meaning; a `Session` is built
//! // only by `new` below, with `rc` at 1; it lives in a `Box`, which the
//! // provided `c_release` frees.
//! unsafe impl CCounted for Session {
//!     type Alloc = std::alloc::Global;
//!     unsafe fn c_up_ref(this: NonNull<Self>) {
//!         // SAFETY: a live session; `rc` is atomic.
//!         unsafe { this.as_ref() }.rc.fetch_add(1, Ordering::Relaxed);
//!     }
//!     unsafe fn c_down_ref(this: NonNull<Self>) -> bool {
//!         // SAFETY: as above.
//!         unsafe { this.as_ref() }.rc.fetch_sub(1, Ordering::AcqRel) == 1
//!     }
//!     unsafe fn c_is_sole(this: NonNull<Self>) -> bool {
//!         // SAFETY: as above.
//!         unsafe { this.as_ref() }.rc.load(Ordering::Acquire) == 1
//!     }
//! }
//! // SAFETY: what C writes, and what holders write while it is shared, is
//! // atomic.
//! unsafe impl CShareable for Session {}
//!
//! impl Session {
//!     pub fn new() -> CShared<Session> {
//!         CShared::from_box(Box::new(Session { rc: AtomicI32::new(1), hits: AtomicU32::new(0) }))
//!     }
//!     pub fn hit(&self) -> u32 {
//!         self.hits.fetch_add(1, Ordering::Relaxed) + 1
//!     }
//! }
//!
//! /// `void session_free(session *)`, exported to C.
//! pub unsafe extern "C" fn session_free(s: *mut Session) {
//!     // SAFETY: C passes NULL or a session whose reference it gives up.
//!     unsafe { CShared::release(s) }
//! }
//!
//! let a = Session::new();
//! let b = a.clone();                        // c_up_ref
//! assert_eq!(a.hit(), 1);
//! assert_eq!(b.hit(), 2);
//! // C takes a reference of its own, and drops it later.
//! let c_ref = b.clone().into_raw();
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

use core::alloc::{Allocator, Layout};
use core::fmt;
use core::marker::PhantomData;
use core::ops::Deref;
use core::ptr::NonNull;

use alloc::boxed::Box;

/// An object whose reference count is a field of its own, which C reaches
/// too: what [`CShared<Self>`] counts with. Its methods take the object's
/// pointer rather than `&self`, so a type that C writes beyond the count,
/// and so is not [`CShareable`], is never covered by a reference.
///
/// # Safety
///
/// - [`c_up_ref`](Self::c_up_ref) adds one counted reference, and
///   [`c_down_ref`](Self::c_down_ref) drops one, with the meaning C's own
///   increment and decrement give the field: a reference either side takes
///   is one the other side can drop. If `Self` is `Send` or `Sync`, both are
///   atomic with respect to C's and to each other.
/// - `c_down_ref` returns `true` exactly when the caller dropped the last
///   reference that keeps the object alive, so that nothing, Rust or C, is
///   left to reach it: at most once per object. A count with an owner
///   answers `false` while the owner holds the object, and the owner
///   releases it later.
/// - [`c_release`](Self::c_release) frees an object for which `c_down_ref`
///   returned `true`, and is what C's own release does then. The provided one
///   drops a `Box<Self, Self::Alloc>`, so every object a `CShared` adopts
///   lives in such a box, or the impl replaces `c_release` with the release
///   that matches its allocation.
/// - A value of `Self` that safe code can hand to [`CShared::from_box`] or
///   [`CShared::try_new`] counts exactly one reference and has no owner: its
///   constructor, through which safe code makes every value, sets the count
///   so.
/// - [`c_is_sole`](Self::c_is_sole) returns `true` only when the caller's
///   reference is the only way anything, Rust or C, reaches the object, and
///   nothing can take another reference from elsewhere.
/// - None of the methods forms a reference covering more of the object than
///   the fields they use, unless `Self` is [`CShareable`].
pub unsafe trait CCounted: Sized {
    /// The allocator the object lives in, which the provided
    /// [`c_release`](Self::c_release) frees it to, and
    /// [`CShared::from_box`] adopts it from.
    type Alloc: Allocator + Default;

    /// Take one more counted reference (C's increment).
    ///
    /// # Safety
    ///
    /// `this` addresses a live object on which the caller holds a reference.
    unsafe fn c_up_ref(this: NonNull<Self>);

    /// Drop one counted reference; `true` if the object must now be
    /// released.
    ///
    /// # Safety
    ///
    /// `this` addresses a live object; the caller gives up a reference it
    /// holds, and does not reach the object afterwards through it.
    unsafe fn c_down_ref(this: NonNull<Self>) -> bool;

    /// Whether the caller's reference is the only one. The provided one
    /// answers `false`, which only withholds [`CShared::get_mut`].
    ///
    /// # Safety
    ///
    /// `this` addresses a live object on which the caller holds a reference.
    #[inline]
    unsafe fn c_is_sole(this: NonNull<Self>) -> bool {
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
        // SAFETY: per the trait's contract, the object lives in a
        // `Box<Self, Self::Alloc>`, which nothing reaches any more.
        drop(unsafe { Box::from_raw_in(this.as_ptr(), Self::Alloc::default()) });
    }
}

/// A [`CCounted`] object that a shared reference may cover while it is
/// shared, with C or other holders: [`CShared`] derefs to it, as `Arc<T>`
/// does.
///
/// # Safety
///
/// While any counted reference to the object lives, every field that C, or
/// a holder through `&Self`, writes is interior-mutable: an atomic, or an
/// `UnsafeCell` behind the type's own synchronisation. Nothing moves or
/// frees a field while a reference lives. `Sync` is the type's own: it
/// states that those writes are synchronised across threads.
pub unsafe trait CShareable: CCounted {}

/// One counted reference to a Rust-allocated object whose count C shares:
/// `Arc<T>`, where the count is the object's own field ([`CCounted`]).
///
/// Cloning takes a reference ([`c_up_ref`](CCounted::c_up_ref)); dropping
/// drops one, and the last frees the object
/// ([`c_release`](CCounted::c_release)), whether the last holder is Rust or
/// C. A reference goes to C with [`into_raw`](Self::into_raw) and comes back
/// with [`from_raw`](Self::from_raw); C's own release routine is
/// [`release`](Self::release), behind the binding's export.
///
/// It derefs to `T` when `T` is [`CShareable`]: a type C writes beyond its
/// count, without interior mutability, is reached through
/// [`as_ptr`](Self::as_ptr) and the binding's own handles, never as `&T`:
///
/// ```compile_fail,E0614
/// # use core::ptr::NonNull;
/// # use core::sync::atomic::{AtomicI32, Ordering};
/// # use ffibox::counted::{CCounted, CShared};
/// # #[repr(C)] pub struct Index { rc: AtomicI32, entries: usize }
/// # unsafe impl CCounted for Index {
/// #     type Alloc = std::alloc::Global;
/// #     unsafe fn c_up_ref(this: NonNull<Self>) {}
/// #     unsafe fn c_down_ref(this: NonNull<Self>) -> bool { true }
/// # }
/// let ix = CShared::from_box(Box::new(Index { rc: AtomicI32::new(1), entries: 0 }));
/// let _ = (*ix).entries; // not `CShareable`: C writes `entries`
/// ```
///
/// `Send` and `Sync` on `Arc`'s terms, `T: Send + Sync`, so a type C uses
/// on one thread at a time stays on its thread:
///
/// ```compile_fail,E0277
/// # use core::cell::Cell;
/// # use core::ptr::NonNull;
/// # use ffibox::counted::{CCounted, CShared};
/// # #[repr(C)] pub struct Walk { rc: Cell<i32> }
/// # unsafe impl CCounted for Walk {
/// #     type Alloc = std::alloc::Global;
/// #     unsafe fn c_up_ref(this: NonNull<Self>) {}
/// #     unsafe fn c_down_ref(this: NonNull<Self>) -> bool { true }
/// # }
/// fn send<T: Send>(_: T) {}
/// send(CShared::from_box(Box::new(Walk { rc: Cell::new(1) })));
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
    /// Share a boxed object, whose one reference becomes this `CShared`:
    /// `Arc::from(Box<T>)`, without copying.
    #[inline]
    pub fn from_box(value: Box<T, T::Alloc>) -> Self {
        let (ptr, _alloc) = Box::into_raw_with_allocator(value);
        Self {
            // SAFETY: from a box, so non-null.
            ptr: unsafe { NonNull::new_unchecked(ptr) },
            _owns: PhantomData,
        }
    }

    /// Allocate `value` in [`T::Alloc`](CCounted::Alloc) and share it:
    /// `Arc::new`, returning the value if the allocation fails.
    pub fn try_new(value: T) -> Result<Self, T> {
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
    /// C passes it), and the caller transfers one counted reference.
    #[inline]
    pub unsafe fn from_raw(ptr: *mut T) -> Option<Self> {
        NonNull::new(ptr).map(|ptr| Self {
            ptr,
            _owns: PhantomData,
        })
    }

    /// Take a new counted reference to an object the caller only borrows:
    /// `Arc::increment_strong_count` then `Arc::from_raw`, as for a pointer C
    /// lends. `None` if `ptr` is null.
    ///
    /// # Safety
    ///
    /// `ptr` is null or addresses a live `T` that a reference someone holds
    /// keeps alive during the call.
    #[inline]
    pub unsafe fn from_borrowed(ptr: *mut T) -> Option<Self> {
        let ptr = NonNull::new(ptr)?;
        // SAFETY: a live object, kept alive by the caller's borrow.
        unsafe { T::c_up_ref(ptr) };
        Some(Self {
            ptr,
            _owns: PhantomData,
        })
    }

    /// The object's pointer, for C to use while `self` lives. The reference
    /// stays with `self`.
    #[inline]
    #[must_use]
    pub fn as_ptr(&self) -> *mut T {
        self.ptr.as_ptr()
    }

    /// Give the reference up, to C or to a later
    /// [`from_raw`](Self::from_raw).
    #[inline]
    #[must_use = "the returned pointer holds a reference that must be released"]
    pub fn into_raw(self) -> *mut T {
        let ptr = self.ptr.as_ptr();
        core::mem::forget(self);
        ptr
    }

    /// Whether two references are to the same object, like `Arc::ptr_eq`.
    #[inline]
    #[must_use]
    pub fn ptr_eq(this: &Self, other: &Self) -> bool {
        this.ptr == other.ptr
    }

    /// The object, exclusively, when this is its only reference
    /// ([`c_is_sole`](CCounted::c_is_sole)), like `Arc::get_mut`.
    #[inline]
    pub fn get_mut(&mut self) -> Option<&mut T> {
        // SAFETY: this reference keeps the object alive.
        if unsafe { T::c_is_sole(self.ptr) } {
            // SAFETY: nothing else reaches the object (`c_is_sole`'s
            // contract), and `&mut self` keeps this reference from being
            // cloned while the borrow lives.
            Some(unsafe { self.ptr.as_mut() })
        } else {
            None
        }
    }

    /// Take a reference for C, which holds `ptr` already:
    /// `Arc::increment_strong_count`, C's increment.
    ///
    /// # Safety
    ///
    /// `ptr` addresses a live `T` on which the caller holds a reference.
    #[inline]
    pub unsafe fn increment_count(ptr: *mut T) {
        // SAFETY: per this function's contract; non-null as it is live.
        unsafe { T::c_up_ref(NonNull::new_unchecked(ptr)) }
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
    /// `free`. A binding exports it under C's name. A panic in `T`'s `Drop`
    /// aborts, as it cannot unwind into C.
    ///
    /// # Safety
    ///
    /// `ptr` is NULL, or addresses a live `T` on which the caller gives up a
    /// reference it holds.
    pub unsafe extern "C" fn release(ptr: *mut T) {
        // SAFETY: per this function's contract.
        drop(unsafe { Self::from_raw(ptr) });
    }
}

impl<T: CCounted> Clone for CShared<T> {
    /// Another reference to the same object, through
    /// [`c_up_ref`](CCounted::c_up_ref).
    #[inline]
    fn clone(&self) -> Self {
        // SAFETY: this reference keeps the object alive.
        unsafe { T::c_up_ref(self.ptr) };
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
        unsafe fn c_up_ref(_: NonNull<Self>) {}
        unsafe fn c_down_ref(_: NonNull<Self>) -> bool {
            false
        }
    }
    assert!(core::mem::size_of::<CShared<Probe>>() == core::mem::size_of::<*mut Probe>());
    assert!(core::mem::size_of::<Option<CShared<Probe>>>() == core::mem::size_of::<*mut Probe>());
};
