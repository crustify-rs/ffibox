//! The handle and owner types: borrowed handle storage ([`CBorrowedPtr`]),
//! the sole owners ([`CBox`], [`CVoidBox`], [`CStrBox`], [`CVec`], [`CVal`]),
//! and the run views ([`CSlice`] / [`CSliceMut`]). The shared owners are in
//! [`shared`](crate::shared). The
//! [README](https://github.com/crustify-rs/ffibox#1-the-types-you-get) says
//! which to pick.
//!
//! ## Nothing ever references a wrapped C object
//!
//! A `&Wrapper` covering a C object's bytes asserts `noalias` / `readonly` /
//! validity over memory C may write through a pointer it kept. So access goes
//! through handles — `FooRef<'a>` / `FooMut<'a>`, one pointer each — and field
//! access projects a raw pointer out of the handle and goes through `addr_of!`
//! / `addr_of_mut!`. `&FooRef` covers one pointer of Rust stack, never the
//! object, which is why `&self` / `&mut self` methods on handles are sound.
//!
//! The layout type `Foo` exists only because the orphan rule forbids
//! implementing [`CCell`] on a `*-sys` crate's type.
//!
//! ## Owners
//!
//! Like `Box` and `Vec`, the owners adopt a raw pointer with an `unsafe`
//! `from_raw` and give one out with a safe `into_raw` / `as_ptr`. Teardown is a type
//! parameter — a policy implementing [`CDrop`], [`CLenDrop`] or [`CDispose`],
//! stored inline — so an owner cannot be named without its destructor, and
//! one C type can have several. A type alias names the pair:
//! `pub type FooBox = CBox<Foo, FooFree>;`.
//!
//! ## Layout
//!
//! The owners are `#[repr(C)]` with the pointer (or value) first. With a ZST
//! policy, [`CBox`] and [`CStrBox`] are pointer-sized and `Option<_>` takes
//! the null niche, so they substitute for a `*mut T` field in a `#[repr(C)]`
//! struct, and [`CVal`] is the size of its value. They are not
//! `#[repr(transparent)]` — the compiler cannot prove a generic policy is a
//! 1-ZST — so passing one *by value* in place of a pointer across `extern "C"`
//! is not ABI-guaranteed. [`CVec`] stores pointer + length, the pointer NULL
//! when empty, as C's `{ T *ptr; size_t len; }`.

use core::ffi::{c_char, c_void};
use core::fmt;
use core::marker::PhantomData;
use core::mem::ManuallyDrop;
use core::ops::{Bound, RangeBounds};
use core::ptr::NonNull;

use crate::traits::{CCell, CDispose, CDrop, CDupClone, CElem, CLenClone, CLenDrop};

// ---------------------------------------------------------------------------
// CBorrowedPtr — the storage behind every generated handle
// ---------------------------------------------------------------------------

/// The storage every generated `FooRef<'a>` / `FooMut<'a>` is transparent
/// over: one pointer, tagged with the borrow's lifetime.
///
/// Covariant in `'a` (a longer borrow coerces to a shorter one) and in `T`,
/// and `Copy`, exactly like `&'a T` — right for a shared handle. An exclusive
/// handle adds a `PhantomData<&'a mut T>` to be invariant in `T`, as
/// [`CCell`]'s contract requires. `Option<CBorrowedPtr<'a, T>>` is a niche `*const T`,
/// so it substitutes for a raw pointer at the FFI seam.
///
/// Wrapper crates rarely name it: it is the field inside their handle types.
#[repr(transparent)]
pub struct CBorrowedPtr<'a, T> {
    ptr: NonNull<T>,
    _borrow: PhantomData<&'a T>,
}

impl<T> Clone for CBorrowedPtr<'_, T> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}
// A shared handle copies like `&T`; `#[derive]` would add a spurious `T: Copy`.
impl<T> Copy for CBorrowedPtr<'_, T> {}

impl<'a, T> CBorrowedPtr<'a, T> {
    /// Wrap a non-null pointer to the wrapper type.
    ///
    /// # Safety
    ///
    /// `p` must address a live, initialised object that outlives `'a`.
    #[inline]
    pub const unsafe fn new(p: NonNull<T>) -> Self {
        Self {
            ptr: p,
            _borrow: PhantomData,
        }
    }

    /// The wrapper pointer this handle borrows.
    #[inline]
    #[must_use]
    pub const fn as_non_null(self) -> NonNull<T> {
        self.ptr
    }
}

/// Build `T`'s shared handle from a pointer to it.
///
/// The one place a handle is made generically: [`CCell`]'s contract makes
/// `T::Ref<'a>` `#[repr(transparent)]` over `CBorrowedPtr<'a, T>`, so the
/// conversion is a layout-preserving copy. Crate-private, so no constructor is
/// reachable through `CCell` from outside.
///
/// # Safety
///
/// `p` must address a live, initialised `T` that outlives `'a`.
#[inline]
pub(crate) unsafe fn handle_ref<'a, T: CCell + 'a>(p: NonNull<T>) -> T::Ref<'a> {
    // Checked at compile time for every `T` this is instantiated with: a
    // `CCell` impl whose handle is not pointer-sized fails the build.
    const {
        assert!(
            core::mem::size_of::<T::Ref<'a>>() == core::mem::size_of::<CBorrowedPtr<'a, T>>(),
            "CCell::Ref must be transparent over CBorrowedPtr",
        )
    };
    // SAFETY: the caller upholds liveness for `'a`.
    let raw = unsafe { CBorrowedPtr::<'a, T>::new(p) };
    // SAFETY: `CCell`'s contract makes `T::Ref<'a>` transparent over
    // `CBorrowedPtr<'a, T>` with no `Drop`; the sizes are equal (checked above),
    // so `transmute_copy` reads exactly the one pointer `raw` holds.
    unsafe { core::mem::transmute_copy(&raw) }
}

/// Build `T`'s exclusive handle from a pointer to it. As [`handle_ref`].
///
/// # Safety
///
/// As [`handle_ref`], plus: `p` must carry write provenance, and no other
/// handle to the object may be used while the result lives.
#[inline]
pub(crate) unsafe fn handle_mut<'a, T: CCell + 'a>(p: NonNull<T>) -> T::Mut<'a> {
    // Checked at compile time for every `T` this is instantiated with: a
    // `CCell` impl whose handle is not pointer-sized fails the build.
    const {
        assert!(
            core::mem::size_of::<T::Mut<'a>>() == core::mem::size_of::<CBorrowedPtr<'a, T>>(),
            "CCell::Mut must be transparent over CBorrowedPtr",
        )
    };
    // SAFETY: the caller upholds liveness for `'a`.
    let raw = unsafe { CBorrowedPtr::<'a, T>::new(p) };
    // SAFETY: as `handle_ref`, for `T::Mut<'a>`.
    unsafe { core::mem::transmute_copy(&raw) }
}

// ---------------------------------------------------------------------------
// Abort helper — portable across std and no_std builds
// ---------------------------------------------------------------------------

/// Abort the process unconditionally. Delegates to [`std::process::abort`]
/// with `std`, and to a double-panic without it.
#[cfg(feature = "std")]
#[cold]
#[inline(never)]
pub(crate) fn abort_process() -> ! {
    std::process::abort()
}

#[cfg(not(feature = "std"))]
#[cold]
#[inline(never)]
pub(crate) fn abort_process() -> ! {
    // No guaranteed abort primitive on stable no_std. A double-panic aborts on
    // every platform: the second fires while the first is unwinding. Call sites
    // carry the context for why aborting is right there.
    struct PanicOnDrop;
    impl Drop for PanicOnDrop {
        fn drop(&mut self) {
            panic!("ffibox: unrecoverable failure in smart-pointer operation — aborting");
        }
    }
    let _guard = PanicOnDrop;
    panic!("ffibox: unrecoverable failure in smart-pointer operation — aborting");
}

// ===========================================================================
// Owners
// ===========================================================================

/// Split an owner into its fields without running its `Drop`.
///
/// # Safety
///
/// `read` must copy each field out of the `ManuallyDrop` exactly once.
#[inline(always)]
pub(crate) unsafe fn disarm<O, R>(owner: O, read: impl FnOnce(&O) -> R) -> R {
    let owner = ManuallyDrop::new(owner);
    read(&owner)
}

// ---------------------------------------------------------------------------
// CBox<T, D> — the sole owner of a C-allocated object
// ---------------------------------------------------------------------------

/// The sole owner of a C-allocated `T`, released on drop by the policy `D` —
/// a `Box` whose destructor is a C routine.
///
/// `T` is a [`define_ctype!`](crate::define_ctype) layout type, which unlocks
/// the handles ([`as_ref`](Self::as_ref) / [`as_mut`](Self::as_mut)) and the
/// C-typed seam ([`from_c`](Self::from_c) / [`into_c`](Self::into_c) /
/// [`as_c_ptr`](Self::as_c_ptr)), or [`c_void`] for an opaque payload (see
/// [`CVoidBox`]).
///
/// **Sole** is the contract: `as_mut` hands out the exclusive handle from
/// `&mut self`, which is sound only if nothing else — another owner, or C —
/// uses the object meanwhile. A refcounted object fits while this is its only
/// reference (a fresh object, with its down-ref as the policy), but `CBox`
/// never shares it: `Clone` is a deep copy through [`CDupClone`], never an
/// `up_ref`.
///
/// `Send` / `Sync` on `Box`'s terms: `T: Send` / `T: Sync`, plus the policy's.
/// `#[repr(C)]` over `{ptr, policy}`: pointer-sized with a ZST policy, with the
/// null niche (see the [module docs](self#layout)).
#[repr(C)]
#[must_use = "dropping a CBox runs its teardown policy"]
pub struct CBox<T, D: CDrop<T>> {
    ptr: NonNull<T>,
    policy: D,
}

impl<T, D: CDrop<T>> CBox<T, D> {
    /// Take ownership of a raw pointer under `D::default()`; `None` if null.
    ///
    /// # Safety
    ///
    /// `ptr` must be null or address a live `T` that nothing else uses while
    /// the box lives, and the caller transfers the ownership `D::c_drop`
    /// releases.
    #[inline]
    pub unsafe fn from_raw(ptr: *mut T) -> Option<Self>
    where
        D: Default,
    {
        // SAFETY: the caller upholds `from_raw`'s contract.
        unsafe { Self::from_raw_with(ptr, D::default()) }
    }

    /// Take ownership of a raw pointer under `policy`; `None` if null.
    ///
    /// # Safety
    ///
    /// As [`from_raw`](Self::from_raw), with `policy` releasing it.
    #[inline]
    pub unsafe fn from_raw_with(ptr: *mut T, policy: D) -> Option<Self> {
        NonNull::new(ptr).map(|ptr| Self { ptr, policy })
    }

    /// Raw pointer for passing to C. Ownership is retained.
    #[inline]
    #[must_use]
    pub fn as_ptr(&self) -> *mut T {
        self.ptr.as_ptr()
    }

    /// The policy that will release the object.
    #[inline]
    #[must_use]
    pub fn policy(&self) -> &D {
        &self.policy
    }

    /// Give up ownership without running the policy. The caller becomes
    /// responsible for releasing the object.
    #[inline]
    #[must_use = "the returned pointer owns the object and must be freed"]
    pub fn into_raw(self) -> *mut T {
        self.into_raw_with().0
    }

    /// As [`into_raw`](Self::into_raw), also returning the policy.
    #[inline]
    #[must_use = "the returned pointer owns the object and must be freed"]
    pub fn into_raw_with(self) -> (*mut T, D) {
        // SAFETY: each field is read out exactly once and `self` is not
        // dropped.
        unsafe { disarm(self, |o| (o.ptr.as_ptr(), core::ptr::read(&o.policy))) }
    }

    /// Swap the teardown policy, returning the old one.
    ///
    /// The construction-phase move: hold an allocation under a storage-only
    /// policy while filling it in, then hand it to the full destructor once
    /// formed. Bail with `?` before promoting and only the storage is freed.
    ///
    /// # Safety
    ///
    /// The object must satisfy every invariant `policy.c_drop` relies on.
    #[inline]
    pub unsafe fn with_policy<E: CDrop<T>>(self, policy: E) -> (CBox<T, E>, D) {
        let (ptr, old) = self.into_raw_with();
        // SAFETY: `ptr` was non-null in `self`; the caller vouches that
        // `policy` releases it.
        let ptr = unsafe { NonNull::new_unchecked(ptr) };
        (CBox { ptr, policy }, old)
    }
}

impl<T: CCell, D: CDrop<T>> CBox<T, D> {
    /// [`from_raw`](Self::from_raw) from the C type's pointer, so adopting a
    /// C constructor's result needs no cast.
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
    #[must_use = "the returned pointer owns the object and must be freed"]
    pub fn into_c(self) -> *mut T::C {
        self.into_raw().cast()
    }

    /// [`as_ptr`](Self::as_ptr) as the C type's pointer.
    #[inline]
    #[must_use]
    pub fn as_c_ptr(&self) -> *mut T::C {
        self.as_ptr().cast()
    }

    /// Shared handle to the object — `Copy`, the getters.
    ///
    /// Not `Deref`: `Deref::Target` cannot name a lifetime taken from `&self`,
    /// and the handle carries one.
    #[inline]
    #[must_use]
    pub fn as_ref(&self) -> T::Ref<'_> {
        // SAFETY: `self.ptr` addresses a live `T` we own; the handle is bound
        // to this borrow.
        unsafe { handle_ref(self.ptr) }
    }

    /// Exclusive handle to the object — the setters.
    #[inline]
    #[must_use]
    pub fn as_mut(&mut self) -> T::Mut<'_> {
        // SAFETY: as `as_ref`, from an exclusive borrow of the sole owner, so
        // no other handle to the object can be in use.
        unsafe { handle_mut(self.ptr) }
    }
}

impl<T, D: CDrop<T>> Drop for CBox<T, D> {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: `self.ptr` is a live `T` we own; the policy releases it once.
        unsafe { self.policy.c_drop(self.ptr) }
    }
}

impl<T, D: CDupClone<T> + Clone> CBox<T, D> {
    /// Deep copy through [`CDupClone::c_dup`]; `None` if the C routine failed.
    ///
    /// [`Clone::clone`] aborts on that failure instead, because `Clone` is
    /// infallible. Use this where the C code checked the `*_dup` result:
    ///
    /// ```ignore
    /// let copy = pkey.try_clone().ok_or(Error::DupFailed)?;
    /// ```
    #[inline]
    pub fn try_clone(&self) -> Option<Self> {
        // SAFETY: `self.ptr` is live; per `CDupClone` a `Some` is a fresh,
        // independent object a clone of the policy releases.
        let ptr = unsafe { self.policy.c_dup(self.ptr) }?;
        Some(Self {
            ptr,
            policy: self.policy.clone(),
        })
    }
}

impl<T, D: CDupClone<T> + Clone> Clone for CBox<T, D> {
    #[inline]
    fn clone(&self) -> Self {
        // Abort rather than fabricate a handle, as `Box` does on OOM; use
        // `try_clone` for the recoverable path.
        match self.try_clone() {
            Some(b) => b,
            None => abort_process(),
        }
    }
}

impl<T, D: CDrop<T>> fmt::Debug for CBox<T, D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("CBox").field(&self.ptr.as_ptr()).finish()
    }
}

impl<T, D: CDrop<T>> fmt::Pointer for CBox<T, D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Pointer::fmt(&self.ptr.as_ptr(), f)
    }
}

// SAFETY: `CBox` is the sole owner of its `T`, as `Box` is: moving it moves
// the only route to the object, and `&CBox` shares only what `T: Sync`
// permits. The policy travels with the pointer and runs on whichever thread
// drops it, so it must cross too.
unsafe impl<T: Send, D: CDrop<T> + Send> Send for CBox<T, D> {}
// SAFETY: as above; `&CBox<T, D>` hands out `T::Ref<'_>` and `&D`.
unsafe impl<T: Sync, D: CDrop<T> + Sync> Sync for CBox<T, D> {}

/// The sole owner of an opaque `void *` payload, released by `D`. Nothing
/// looks inside it; the wrapper passes [`as_ptr`](CBox::as_ptr) to C.
///
/// [`c_void`] is `Send + Sync`, so a payload's thread-safety is its policy's
/// alone — and a unit-struct policy is `Send + Sync`, so by default the box
/// crosses threads and its free runs wherever it drops. A payload that must
/// stay on one thread opts out with a policy carrying `PhantomData<*const ()>`.
pub type CVoidBox<D> = CBox<c_void, D>;

// ---------------------------------------------------------------------------
// CStrBox<D> — owned NUL-terminated C string
// ---------------------------------------------------------------------------

/// Owned, **NUL-terminated** C string released on drop by the policy `D` — a
/// `CString` freed by a C allocator rather than Rust's.
///
/// Separate from `CBox<c_char, D>` because the terminator is a type
/// invariant, and it is what makes the views ([`as_c_str`](Self::as_c_str) /
/// [`as_bytes`](Self::as_bytes) / [`to_str`](Self::to_str)) safe. They are
/// read-only, like [`CStr`](core::ffi::CStr): a `&mut [u8]` could clobber the
/// terminator.
///
/// `Send` / `Sync` follow the policy alone, which names the allocator. A
/// unit-struct policy is `Send + Sync`, so by default the string crosses
/// threads and is freed wherever it drops; a policy for an allocator that must
/// free on the allocating thread opts out by carrying `PhantomData<*const ()>`.
#[repr(C)]
#[must_use = "dropping a CStrBox runs its teardown policy"]
pub struct CStrBox<D: CDrop<c_char>> {
    ptr: NonNull<c_char>,
    policy: D,
}

impl<D: CDrop<c_char>> CStrBox<D> {
    /// Take ownership of a C string under `D::default()`; `None` if null.
    ///
    /// # Safety
    ///
    /// `ptr` must be null or a valid, NUL-terminated, uniquely-owned string
    /// that `D::c_drop` releases.
    #[inline]
    pub unsafe fn from_raw(ptr: *mut c_char) -> Option<Self>
    where
        D: Default,
    {
        // SAFETY: the caller upholds `from_raw`'s contract.
        unsafe { Self::from_raw_with(ptr, D::default()) }
    }

    /// Take ownership of a C string under `policy`; `None` if null.
    ///
    /// # Safety
    ///
    /// As [`from_raw`](Self::from_raw), with `policy` releasing it.
    #[inline]
    pub unsafe fn from_raw_with(ptr: *mut c_char, policy: D) -> Option<Self> {
        NonNull::new(ptr).map(|ptr| Self { ptr, policy })
    }

    /// The string for passing to C. Ownership is retained.
    ///
    /// `*const`, like [`CStr::as_ptr`](core::ffi::CStr::as_ptr), because the
    /// views are read-only; [`into_raw`](Self::into_raw) is the `*mut` path.
    #[inline]
    #[must_use]
    pub fn as_ptr(&self) -> *const c_char {
        self.ptr.as_ptr()
    }

    /// Give up ownership without freeing. Reclaim the string with
    /// [`from_raw`](Self::from_raw) or free it in C; otherwise it leaks.
    #[inline]
    #[must_use = "the returned pointer owns the string and must be freed"]
    pub fn into_raw(self) -> *mut c_char {
        self.into_raw_with().0
    }

    /// As [`into_raw`](Self::into_raw), also returning the policy.
    #[inline]
    #[must_use = "the returned pointer owns the string and must be freed"]
    pub fn into_raw_with(self) -> (*mut c_char, D) {
        // SAFETY: each field is read out exactly once and `self` is not
        // dropped.
        unsafe { disarm(self, |o| (o.ptr.as_ptr(), core::ptr::read(&o.policy))) }
    }

    /// Borrowed [`CStr`](core::ffi::CStr) view (computes `strlen`).
    #[inline]
    #[must_use]
    pub fn as_c_str(&self) -> &core::ffi::CStr {
        // SAFETY: the type invariant guarantees a live, NUL-terminated string
        // at `self.ptr`; the view is bound to `&self`.
        unsafe { core::ffi::CStr::from_ptr(self.as_ptr()) }
    }

    /// The bytes, **excluding** the NUL, like
    /// [`CStr::to_bytes`](core::ffi::CStr::to_bytes).
    #[inline]
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        self.as_c_str().to_bytes()
    }

    /// The `strlen`, recomputed on every call.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.as_bytes().len()
    }

    /// Whether the first byte is the NUL. O(1).
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        // SAFETY: the type invariant guarantees a live, NUL-terminated string,
        // so its first byte is readable.
        unsafe { self.ptr.as_ptr().read() == 0 }
    }

    /// Decode as UTF-8. `Err` if the bytes are not UTF-8.
    #[inline]
    pub fn to_str(&self) -> Result<&str, core::str::Utf8Error> {
        self.as_c_str().to_str()
    }
}

impl<D: CDrop<c_char>> Drop for CStrBox<D> {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: we uniquely own the string; the policy frees it once.
        unsafe { self.policy.c_drop(self.ptr) }
    }
}

impl<D: CDupClone<c_char> + Clone> CStrBox<D> {
    /// Deep copy (`strdup`) through the policy; `None` if the C copy failed.
    #[inline]
    pub fn try_clone(&self) -> Option<Self> {
        // SAFETY: `self.ptr` is a live NUL-terminated string; per `CDupClone`
        // a `Some` is a fresh copy a clone of the policy releases.
        let ptr = unsafe { self.policy.c_dup(self.ptr) }?;
        Some(Self {
            ptr,
            policy: self.policy.clone(),
        })
    }
}

impl<D: CDupClone<c_char> + Clone> Clone for CStrBox<D> {
    #[inline]
    fn clone(&self) -> Self {
        // Abort on a failed copy; see `CBox::clone`.
        match self.try_clone() {
            Some(s) => s,
            None => abort_process(),
        }
    }
}

impl<D: CDrop<c_char>> fmt::Debug for CStrBox<D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("CStrBox").field(&self.as_c_str()).finish()
    }
}

impl<D: CDrop<c_char>> fmt::Pointer for CStrBox<D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Pointer::fmt(&self.as_ptr(), f)
    }
}

// SAFETY: the string is owned bytes with no interior mutability; the only
// cross-thread question is whether the allocator frees from another thread,
// which the policy answers by being `Send` or not.
unsafe impl<D: CDrop<c_char> + Send> Send for CStrBox<D> {}
// SAFETY: `&CStrBox` hands out only read-only views and `&D`.
unsafe impl<D: CDrop<c_char> + Sync> Sync for CStrBox<D> {}

// ---------------------------------------------------------------------------
// CVec<T, S> — owned counted buffer
// ---------------------------------------------------------------------------

/// Owned C-allocated array of `len` elements, released on drop by the policy's
/// [`c_drop_len`](CLenDrop::c_drop_len) with the total byte length.
///
/// Plain elements ([`CElem`]) read out as a real `&[T]`, which is sound
/// because the buffer is owned exclusively. Wrapped C objects come out as
/// handles through [`as_handles`](Self::as_handles), since `&[Foo]` would be a
/// reference covering them.
///
/// **NULL with length 0 is a valid, empty `CVec`**, which C commonly returns
/// for an empty array: adopt it with
/// [`from_raw_parts_or_empty`](Self::from_raw_parts_or_empty), or make one with
/// [`empty`](Self::empty). It frees nothing on drop and hands NULL back to C
/// through [`as_ptr`](Self::as_ptr) / [`into_raw_parts`](Self::into_raw_parts),
/// so the layout stays C's `{ T *ptr; size_t len; }`. The length alone cannot
/// mark it: a non-null zero-length buffer (`malloc(0)`) still has to be freed.
///
/// Byte-wise cloning needs `T: Copy` and a [`CLenClone`] policy. Wrapped C
/// objects do not meet that bound: copying their bytes would duplicate the
/// ownership they embed.
///
/// ```compile_fail
/// use core::ptr::NonNull;
/// use ffibox::{define_ctype, CLenClone, CLenDrop, CVec};
///
/// #[repr(C)]
/// pub struct RawObject {
///     owned: *mut u8,
/// }
/// define_ctype!(Object, ObjectRef, ObjectMut, RawObject);
///
/// #[derive(Clone)]
/// struct Memdup;
/// unsafe impl CLenDrop for Memdup {
///     unsafe fn c_drop_len(&self, _: *mut u8, _: usize) {}
/// }
/// unsafe impl CLenClone for Memdup {
///     unsafe fn c_clone_len(&self, _: *mut u8, _: usize) -> Option<NonNull<u8>> {
///         unimplemented!()
///     }
/// }
///
/// fn requires_clone<T: Clone>() {}
/// requires_clone::<CVec<Object, Memdup>>();
/// ```
///
/// `Send` / `Sync` on `Vec`'s terms, plus the policy's. A `CVec` of wrapped C
/// objects is `!Send` until their layout type opts in; a `CVec` of plain
/// elements (`u8`, `u32`, raw pointers) under a unit-struct policy is
/// `Send + Sync` by default, so its free runs wherever it drops. A policy for
/// an allocator that must free on the allocating thread opts out by carrying
/// `PhantomData<*const ()>`.
#[repr(C)]
pub struct CVec<T, S: CLenDrop> {
    /// `None` is a NULL buffer: `len` is 0 and there is nothing to free.
    /// `Option<NonNull<T>>` is a nullable `*mut T`, so the layout is C's.
    ptr: Option<NonNull<T>>,
    len: usize,
    policy: S,
}

impl<T, S: CLenDrop> CVec<T, S> {
    /// Take ownership of `len` elements at `ptr` under `S::default()`; `None`
    /// if null. Use [`from_raw_parts_or_empty`](Self::from_raw_parts_or_empty)
    /// where C returns NULL for an empty array.
    ///
    /// # Safety
    ///
    /// - `ptr` must be null or hold `len` contiguous, initialised, properly
    ///   aligned `T` from the allocator the policy frees, and the caller
    ///   transfers unique ownership.
    /// - [`byte_len`](Self::byte_len) — `len * size_of::<T>()`, what the
    ///   policy receives on drop — must meet
    ///   [`CLenDrop::c_drop_len`]'s contract: no larger than the allocation,
    ///   and exactly its size when the policy's free takes a size. A buffer
    ///   with spare capacity beyond `len` therefore cannot go under a
    ///   size-taking policy.
    ///
    /// Nothing here is checked at run time: an allocation's size is known only
    /// to its allocator.
    #[inline]
    pub unsafe fn from_raw_parts(ptr: *mut T, len: usize) -> Option<Self>
    where
        S: Default,
    {
        // SAFETY: the caller upholds `from_raw_parts`'s contract.
        unsafe { Self::from_raw_parts_with(ptr, len, S::default()) }
    }

    /// Take ownership under `policy`; `None` if null.
    ///
    /// # Safety
    ///
    /// As [`from_raw_parts`](Self::from_raw_parts), with `policy` freeing it.
    #[inline]
    pub unsafe fn from_raw_parts_with(ptr: *mut T, len: usize, policy: S) -> Option<Self> {
        NonNull::new(ptr).map(|ptr| Self {
            ptr: Some(ptr),
            len,
            policy,
        })
    }

    /// As [`from_raw_parts`](Self::from_raw_parts), but NULL with `len == 0`
    /// is an empty buffer rather than `None`; NULL with a length is still
    /// `None`.
    ///
    /// Kept apart from `from_raw_parts` because some C APIs also return NULL
    /// and 0 on *failure*: which one a NULL means is the wrapper's call.
    ///
    /// # Safety
    ///
    /// As [`from_raw_parts`](Self::from_raw_parts) for a non-null `ptr`.
    #[inline]
    pub unsafe fn from_raw_parts_or_empty(ptr: *mut T, len: usize) -> Option<Self>
    where
        S: Default,
    {
        // SAFETY: the caller upholds `from_raw_parts`'s contract.
        unsafe { Self::from_raw_parts_or_empty_with(ptr, len, S::default()) }
    }

    /// As [`from_raw_parts_or_empty`](Self::from_raw_parts_or_empty), under
    /// `policy`.
    ///
    /// # Safety
    ///
    /// As [`from_raw_parts`](Self::from_raw_parts) for a non-null `ptr`, with
    /// `policy` freeing it.
    #[inline]
    pub unsafe fn from_raw_parts_or_empty_with(ptr: *mut T, len: usize, policy: S) -> Option<Self> {
        match NonNull::new(ptr) {
            Some(ptr) => Some(Self {
                ptr: Some(ptr),
                len,
                policy,
            }),
            None if len == 0 => Some(Self::empty_with(policy)),
            None => None,
        }
    }

    /// An empty buffer under `S::default()`: NULL, length 0, nothing to free.
    #[inline]
    #[must_use]
    pub fn empty() -> Self
    where
        S: Default,
    {
        Self::empty_with(S::default())
    }

    /// An empty buffer under `policy`.
    #[inline]
    #[must_use]
    pub const fn empty_with(policy: S) -> Self {
        Self {
            ptr: None,
            len: 0,
            policy,
        }
    }

    /// Raw pointer to the first element — NULL for an empty buffer adopted
    /// from NULL. Ownership is retained.
    #[inline]
    #[must_use]
    pub fn as_ptr(&self) -> *mut T {
        self.ptr.map_or(core::ptr::null_mut(), NonNull::as_ptr)
    }

    /// The elements' base for Rust-side views: dangling, as for an empty
    /// `Vec`, when the buffer is NULL — a slice may not start at NULL.
    #[inline]
    fn data(&self) -> NonNull<T> {
        self.ptr.unwrap_or(NonNull::dangling())
    }

    /// Number of elements.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the buffer holds no elements.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Total size in bytes, as passed to the policy.
    #[inline]
    #[must_use]
    pub fn byte_len(&self) -> usize {
        self.len.wrapping_mul(core::mem::size_of::<T>())
    }

    /// Give up ownership without freeing: pointer and element count.
    #[inline]
    #[must_use = "the returned pointer owns the allocation and must be freed"]
    pub fn into_raw_parts(self) -> (*mut T, usize) {
        let (ptr, len, _) = self.into_raw_parts_with();
        (ptr, len)
    }

    /// As [`into_raw_parts`](Self::into_raw_parts), also returning the policy.
    #[inline]
    #[must_use = "the returned pointer owns the allocation and must be freed"]
    pub fn into_raw_parts_with(self) -> (*mut T, usize, S) {
        // SAFETY: each field is read out exactly once and `self` is not
        // dropped.
        unsafe { disarm(self, |o| (o.as_ptr(), o.len, core::ptr::read(&o.policy))) }
    }
}

impl<T: CElem, S: CLenDrop> CVec<T, S> {
    /// The elements as a slice.
    #[inline]
    #[must_use]
    pub fn as_slice(&self) -> &[T] {
        // SAFETY: `len` initialised elements at `data()` per `from_raw_parts`
        // (none, at an aligned dangling pointer, when NULL), each valid by
        // `T: CElem`, owned exclusively; bound by `&self`.
        unsafe { core::slice::from_raw_parts(self.data().as_ptr(), self.len) }
    }

    /// The elements as a mutable slice.
    #[inline]
    #[must_use]
    pub fn as_mut_slice(&mut self) -> &mut [T] {
        // SAFETY: as `as_slice`, with `&mut self` making it exclusive.
        unsafe { core::slice::from_raw_parts_mut(self.data().as_ptr(), self.len) }
    }
}

impl<T: CCell, S: CLenDrop> CVec<T, S> {
    /// The elements as a run of shared handles.
    #[inline]
    #[must_use]
    pub fn as_handles(&self) -> CSlice<'_, T> {
        // SAFETY: `len` initialised `T` at `data()` (none when NULL); the view
        // is bound by `&self`.
        unsafe { CSlice::from_raw_parts(self.data(), self.len) }
    }

    /// The elements as a run of exclusive handles.
    #[inline]
    #[must_use]
    pub fn as_handles_mut(&mut self) -> CSliceMut<'_, T> {
        // SAFETY: as `as_handles`; `&mut self` and exclusive ownership make
        // this the only route to the elements meanwhile.
        unsafe { CSliceMut::from_raw_parts(self.data(), self.len) }
    }
}

impl<T, S: CLenDrop> Drop for CVec<T, S> {
    #[inline]
    fn drop(&mut self) {
        // A NULL buffer owns no allocation; some frees reject NULL.
        if let Some(ptr) = self.ptr {
            // SAFETY: `byte_len` bytes at `ptr` from the policy's allocator.
            unsafe { self.policy.c_drop_len(ptr.as_ptr().cast(), self.byte_len()) }
        }
    }
}

impl<T: Copy, S: CLenClone + Clone> CVec<T, S> {
    /// Byte copy into a fresh allocation through the policy; `None` if the C
    /// copy failed. A NULL buffer clones to another, without calling C.
    #[inline]
    pub fn try_clone(&self) -> Option<Self> {
        let Some(src) = self.ptr else {
            return Some(Self::empty_with(self.policy.clone()));
        };
        // SAFETY: `byte_len()` live bytes at `src`; per `CLenClone` a `Some`
        // is a fresh copy the policy releases.
        let ptr = unsafe {
            self.policy
                .c_clone_len(src.as_ptr().cast(), self.byte_len())
        }?;
        Some(Self {
            ptr: Some(ptr.cast()),
            len: self.len,
            policy: self.policy.clone(),
        })
    }
}

impl<T: Copy, S: CLenClone + Clone> Clone for CVec<T, S> {
    #[inline]
    fn clone(&self) -> Self {
        // Abort on a failed copy; see `CBox::clone`.
        match self.try_clone() {
            Some(v) => v,
            None => abort_process(),
        }
    }
}

impl<T, S: CLenDrop> fmt::Debug for CVec<T, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CVec")
            .field("ptr", &self.as_ptr())
            .field("len", &self.len)
            .finish()
    }
}

impl<T, S: CLenDrop + Default> Default for CVec<T, S> {
    /// [`CVec::empty`].
    #[inline]
    fn default() -> Self {
        Self::empty()
    }
}

// SAFETY: `CVec` owns its elements exclusively, as `Vec` does; the policy
// frees on whichever thread drops it, so it must cross too.
unsafe impl<T: Send, S: CLenDrop + Send> Send for CVec<T, S> {}
// SAFETY: `&CVec` hands out `&[T]` / shared handles and nothing mutable.
unsafe impl<T: Sync, S: CLenDrop + Sync> Sync for CVec<T, S> {}

// ---------------------------------------------------------------------------
// CVal<T, D> — a C struct held by value, disposed on drop
// ---------------------------------------------------------------------------

/// A C struct Rust holds **by value** that owns resources, disposed on drop
/// by the policy's [`c_dispose`](CDispose::c_dispose) — `*_uninit`,
/// `*_clear`, which release the fields and leave the storage, Rust's own,
/// alone.
///
/// ```ignore
/// define_ctype!(ChannelLayout, ChannelLayoutRef, ChannelLayoutMut, ffi::AVChannelLayout);
/// #[derive(Clone, Copy, Debug, Default)]
/// pub struct LayoutUninit;
/// impl_cdispose!(LayoutUninit, ChannelLayout, ffi::av_channel_layout_uninit);
///
/// let mut layout: CVal<ChannelLayout, LayoutUninit> = CVal::new(ChannelLayout::zeroed());
/// ```
///
/// A resource-free struct needs no `CVal`: hold the layout type itself. A
/// bare `Foo` embedded in a parent C struct is left to the parent's teardown.
///
/// **The setters must keep the value disposable.** A `CVal` hands its
/// exclusive handle to safe code, and whatever the handle's safe setters can
/// write reaches [`c_dispose`](CDispose::c_dispose) on drop. A setter that
/// could store a pointer the routine would then free, or a length past the
/// buffer it clears, must validate or be `unsafe`; see [`CDispose`]'s contract.
///
/// **Moving a `CVal` moves its bytes**, which is unsound for an
/// address-sensitive struct — one pointing into itself, or one C recorded.
/// Those belong behind a pointer, in a [`CBox`].
#[repr(C)]
#[must_use = "dropping a CVal disposes its resources"]
pub struct CVal<T: CCell, D: CDispose<T>> {
    value: T,
    policy: D,
}

impl<T: CCell, D: CDispose<T>> CVal<T, D> {
    /// Take ownership of a value; dropping the result disposes it.
    #[inline]
    pub fn new(value: T) -> Self
    where
        D: Default,
    {
        Self::new_with(value, D::default())
    }

    /// Take ownership of a value under `policy`.
    #[inline]
    pub fn new_with(value: T, policy: D) -> Self {
        Self { value, policy }
    }

    /// Shared handle to the value — the getters.
    #[inline]
    #[must_use]
    pub fn as_ref(&self) -> T::Ref<'_> {
        // SAFETY: `self.value` is a live, Rust-owned `T`; the handle is bound
        // to this borrow.
        unsafe { handle_ref(NonNull::from(&self.value)) }
    }

    /// Exclusive handle to the value — the setters, and the pointer for C
    /// calls that write it.
    #[inline]
    #[must_use]
    pub fn as_mut(&mut self) -> T::Mut<'_> {
        // SAFETY: as `as_ref`, from an exclusive borrow.
        unsafe { handle_mut(NonNull::from(&mut self.value)) }
    }

    /// The policy that will dispose the value.
    #[inline]
    #[must_use]
    pub fn policy(&self) -> &D {
        &self.policy
    }

    /// Give up disposal and return the bare value, e.g. to move it into a
    /// C-owned parent whose teardown disposes it.
    #[inline]
    pub fn into_inner(self) -> T {
        // SAFETY: each field is read out exactly once and `self` is not
        // dropped; the policy is dropped normally on return.
        let (value, _policy) = unsafe {
            disarm(self, |o| {
                (core::ptr::read(&o.value), core::ptr::read(&o.policy))
            })
        };
        value
    }
}

impl<T: CCell, D: CDispose<T>> Drop for CVal<T, D> {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: `self.value` is a live value this owner holds exclusively;
        // the policy disposes its resources once without freeing the storage.
        unsafe { self.policy.c_dispose(NonNull::from(&mut self.value)) }
    }
}

impl<T: CCell, D: CDispose<T>> fmt::Debug for CVal<T, D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CVal").finish_non_exhaustive()
    }
}

// ===========================================================================
// Run views — borrowed runs of contiguous elements, never a `&[T]`
// ===========================================================================

// ---------------------------------------------------------------------------
// CSlice<'a, T> — a borrowed run of wrapped C objects
// ---------------------------------------------------------------------------

/// A borrowed run of `len` contiguous wrapped C objects, yielded as handles.
///
/// The slice analogue of a `Ref` handle, and for the same reason: `&[T]` over
/// wrapped C objects would be a reference covering them, asserting `noalias` /
/// `readonly` / validity over memory C may write. A `CSlice` is a pointer and a
/// count, so it asserts nothing; [`get`](CSlice::get) and [`iter`](CSlice::iter)
/// hand out per-element handles.
///
/// Reached with [`CVec::as_handles`](crate::CVec::as_handles). A buffer of plain
/// Rust values takes [`CVec::as_slice`](crate::CVec::as_slice) instead, which is
/// a real `&[T]`.
pub struct CSlice<'a, T> {
    ptr: NonNull<T>,
    len: usize,
    _borrow: PhantomData<&'a T>,
}

impl<T> Clone for CSlice<'_, T> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}
// Copies like `&[T]`; `#[derive]` would add a spurious `T: Copy`.
impl<T> Copy for CSlice<'_, T> {}

impl<T> fmt::Debug for CSlice<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CSlice").field("len", &self.len).finish()
    }
}

// SAFETY: a shared run is `&[T]`-like: sending or sharing it shares the
// elements, which `T: Sync` permits.
unsafe impl<T: Sync> Send for CSlice<'_, T> {}
// SAFETY: as above.
unsafe impl<T: Sync> Sync for CSlice<'_, T> {}

impl<'a, T> CSlice<'a, T> {
    /// Borrow `len` contiguous elements starting at `ptr`.
    ///
    /// # Safety
    ///
    /// `ptr` must address `len` contiguous, initialised `T` that outlive `'a`.
    #[inline]
    pub const unsafe fn from_raw_parts(ptr: NonNull<T>, len: usize) -> Self {
        Self {
            ptr,
            len,
            _borrow: PhantomData,
        }
    }

    /// Number of elements.
    #[inline]
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether the run is empty.
    #[inline]
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The run split into `[0, mid)` and `[mid, len)`; `None` if `mid > len`.
    ///
    /// Both halves keep `'a`: a shared view is `Copy`, so nothing is frozen.
    #[inline]
    #[must_use]
    pub fn split_at(&self, mid: usize) -> Option<(CSlice<'a, T>, CSlice<'a, T>)> {
        if mid > self.len {
            return None;
        }
        // SAFETY: `mid <= len`, so both halves lie within this view's run,
        // which outlives `'a` per the constructor.
        Some(unsafe {
            (
                CSlice::from_raw_parts(self.ptr, mid),
                CSlice::from_raw_parts(self.ptr.add(mid), self.len - mid),
            )
        })
    }

    /// The sub-run `range`, like `&s[range]`; `None` if out of range.
    #[inline]
    #[must_use]
    pub fn slice(&self, range: impl RangeBounds<usize>) -> Option<CSlice<'a, T>> {
        let (start, end) = sub_range(self.len, range)?;
        // SAFETY: `start <= end <= len`, so the sub-run lies within this
        // view's run, which outlives `'a` per the constructor.
        Some(unsafe { CSlice::from_raw_parts(self.ptr.add(start), end - start) })
    }
}

/// `range` resolved against a run of `len` elements: `Some((start, end))` with
/// `start <= end <= len`, or `None` if it does not fit.
#[inline]
fn sub_range(len: usize, range: impl RangeBounds<usize>) -> Option<(usize, usize)> {
    let start = match range.start_bound() {
        Bound::Included(&s) => s,
        Bound::Excluded(&s) => s.checked_add(1)?,
        Bound::Unbounded => 0,
    };
    let end = match range.end_bound() {
        Bound::Included(&e) => e.checked_add(1)?,
        Bound::Excluded(&e) => e,
        Bound::Unbounded => len,
    };
    (start <= end && end <= len).then_some((start, end))
}

/// A run of wrapped C objects is reached as per-element handles.
impl<'a, T: CCell> CSlice<'a, T> {
    /// Shared handle to element `i`; `None` if out of range.
    ///
    /// The handle carries `'a`, not a lifetime from `&self`, and that is sound
    /// *here* precisely because this view is the shared one: it is `Copy`, so a
    /// caller can hold as many as it likes either way, and none of them grants
    /// write access. The exclusive view must not do this — see
    /// [`CSliceMut::get_mut`].
    #[inline]
    #[must_use]
    pub fn get(&self, i: usize) -> Option<T::Ref<'a>>
    where
        T: 'a,
    {
        if i >= self.len {
            return None;
        }
        // SAFETY: `i < len`, and the constructor guarantees `len` contiguous
        // initialised `T` living for `'a`.
        Some(unsafe { handle_ref(NonNull::new_unchecked(self.ptr.as_ptr().add(i))) })
    }

    /// Iterate the run as shared handles.
    #[inline]
    pub fn iter(&self) -> impl Iterator<Item = T::Ref<'a>> + use<'a, T>
    where
        T: 'a,
    {
        let (ptr, len) = (self.ptr, self.len);
        (0..len).map(move |i| {
            // SAFETY: `i < len`, per the constructor's contract.
            unsafe { handle_ref(NonNull::new_unchecked(ptr.as_ptr().add(i))) }
        })
    }

    /// Read-only pointer to the first element, for passing the run to C.
    ///
    /// `*const`, like [`<[T]>::as_ptr`](slice::as_ptr) and a shared handle's
    /// `as_ptr`: a C routine that is not const-correct takes `.cast_mut()`,
    /// which keeps the write visible at the call site.
    #[inline]
    #[must_use]
    pub fn as_ptr(&self) -> *const T::C {
        self.ptr.as_ptr().cast::<T::C>()
    }
}

/// A run of plain values is read out element-wise, still without a `&[T]`.
///
/// [`CElem`] is what licenses [`CVec::as_slice`](crate::CVec::as_slice) to hand
/// out a real `&[T]`, and it is not enough here — it answers *is every bit
/// pattern a valid `T`*, while a `&[T]` also asserts `noalias` and `readonly`
/// over the whole run for the whole borrow. `CVec` earns those by owning its
/// buffer exclusively. A run inside a C object does not: the library keeps the
/// pointer and may write through it, and no Rust lifetime constrains that. So
/// the element type is not what decides between `&[T]` and a `CSlice` — the
/// owner is.
impl<'a, T: CElem> CSlice<'a, T> {
    /// Copy element `i` out; `None` if out of range.
    #[inline]
    #[must_use]
    pub fn elem(&self, i: usize) -> Option<T>
    where
        T: Copy,
    {
        if i >= self.len {
            return None;
        }
        // SAFETY: `i < len`, the constructor guarantees an initialised `T`
        // there, and `T: CElem` makes every bit pattern a valid one. The read
        // is a copy — no reference over C's memory is formed.
        Some(unsafe { self.ptr.as_ptr().add(i).read() })
    }

    /// Iterate copies of the elements.
    #[inline]
    pub fn elems(&self) -> impl Iterator<Item = T> + use<'a, T>
    where
        T: Copy,
    {
        let (ptr, len) = (self.ptr, self.len);
        // SAFETY: as `elem`, for each `i < len`.
        (0..len).map(move |i| unsafe { ptr.as_ptr().add(i).read() })
    }

    /// Copy the whole run into `dst`. `false` — and nothing copied — if the
    /// lengths differ.
    #[inline]
    #[must_use]
    pub fn copy_to_slice(&self, dst: &mut [T]) -> bool
    where
        T: Copy,
    {
        if dst.len() != self.len {
            return false;
        }
        // SAFETY: `len` initialised `T` at `ptr` per the constructor, `dst` is
        // a live slice of the same length, and the two cannot overlap — `dst`
        // is a Rust reference, which may not cover the C storage this views.
        unsafe { core::ptr::copy_nonoverlapping(self.ptr.as_ptr(), dst.as_mut_ptr(), self.len) };
        true
    }

    /// Read-only pointer to the first element, for passing the run to C.
    /// `*const`, as [`as_ptr`](Self::as_ptr).
    #[inline]
    #[must_use]
    pub fn as_elem_ptr(&self) -> *const T {
        self.ptr.as_ptr()
    }
}

// ---------------------------------------------------------------------------
// CSliceMut<'a, T> — the exclusive run
// ---------------------------------------------------------------------------

/// A borrowed run of `len` contiguous elements, exclusively.
///
/// The `Mut` handle's analogue at slice granularity, and the destination for a
/// `&mut [T]` that would otherwise cover memory C writes. Move-only rather than
/// `Copy`, because that is what exclusivity means; reach the shared view with
/// [`as_ref`](CSliceMut::as_ref), which binds it to the borrow.
pub struct CSliceMut<'a, T> {
    ptr: NonNull<T>,
    len: usize,
    _borrow: PhantomData<&'a mut T>,
}

impl<T> fmt::Debug for CSliceMut<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CSliceMut").field("len", &self.len).finish()
    }
}

// SAFETY: an exclusive run is `&mut [T]`-like: sending it moves exclusive
// access (`T: Send`); sharing it shares only the read paths (`T: Sync`).
unsafe impl<T: Send> Send for CSliceMut<'_, T> {}
// SAFETY: as above.
unsafe impl<T: Sync> Sync for CSliceMut<'_, T> {}

impl<'a, T> CSliceMut<'a, T> {
    /// Borrow `len` contiguous elements exclusively, starting at `ptr`.
    ///
    /// # Safety
    ///
    /// `ptr` must address `len` contiguous, initialised `T` that outlive `'a`,
    /// and no other handle or reference to any of them may be used while the
    /// result lives.
    #[inline]
    pub const unsafe fn from_raw_parts(ptr: NonNull<T>, len: usize) -> Self {
        Self {
            ptr,
            len,
            _borrow: PhantomData,
        }
    }

    /// Number of elements.
    #[inline]
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether the run is empty.
    #[inline]
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Reborrow shared, for passing where a read-only run is wanted.
    ///
    /// Bound by `&self`, so the shared view — and every copy of it — keeps this
    /// one immutably borrowed and no write path is reachable meanwhile. This is
    /// the operation `Deref` cannot express, since `Deref::Target` cannot name
    /// a lifetime taken from `&self`.
    #[inline]
    #[must_use]
    pub fn as_ref(&self) -> CSlice<'_, T> {
        // SAFETY: this view's own contract gives `len` initialised `T` at
        // `ptr`; the result is bound by `&self`, so it cannot outlive it.
        unsafe { CSlice::from_raw_parts(self.ptr, self.len) }
    }

    /// Reborrow exclusively, for passing to a function that takes the view by
    /// value while keeping this one — what `&mut [T]` does implicitly. This
    /// view is frozen while the result lives:
    ///
    /// ```compile_fail,E0499
    /// # use ffibox::CSliceMut;
    /// fn demo(mut s: CSliceMut<'_, u32>) {
    ///     let inner = s.as_mut();
    ///     let _ = s.set_elem(0, 1); // second exclusive use while `inner` lives
    ///     drop(inner);
    /// }
    /// ```
    #[inline]
    #[must_use]
    pub fn as_mut(&mut self) -> CSliceMut<'_, T> {
        // SAFETY: the run is this view's, and the result is bound by
        // `&mut self`, so this view is unusable while it lives.
        unsafe { CSliceMut::from_raw_parts(self.ptr, self.len) }
    }

    /// The run split into exclusive views of `[0, mid)` and `[mid, len)`, like
    /// `<[T]>::split_at_mut`; `None` if `mid > len`. Both are bound by
    /// `&mut self` and cover disjoint elements, so they may be used together.
    #[inline]
    #[must_use]
    pub fn split_at_mut(&mut self, mid: usize) -> Option<(CSliceMut<'_, T>, CSliceMut<'_, T>)> {
        if mid > self.len {
            return None;
        }
        // SAFETY: `mid <= len`, so the halves are disjoint and within this
        // view's run; both are bound by `&mut self`, which freezes this view.
        Some(unsafe {
            (
                CSliceMut::from_raw_parts(self.ptr, mid),
                CSliceMut::from_raw_parts(self.ptr.add(mid), self.len - mid),
            )
        })
    }

    /// The exclusive sub-run `range`, like `&mut s[range]`; `None` if out of
    /// range.
    #[inline]
    #[must_use]
    pub fn slice_mut(&mut self, range: impl RangeBounds<usize>) -> Option<CSliceMut<'_, T>> {
        let (start, end) = sub_range(self.len, range)?;
        // SAFETY: `start <= end <= len`, within this view's run; bound by
        // `&mut self`, which freezes this view.
        Some(unsafe { CSliceMut::from_raw_parts(self.ptr.add(start), end - start) })
    }
}

/// A run of wrapped C objects is reached as per-element handles.
impl<'a, T: CCell> CSliceMut<'a, T> {
    /// Shared handle to element `i`; `None` if out of range.
    #[inline]
    #[must_use]
    pub fn get(&self, i: usize) -> Option<T::Ref<'_>> {
        if i >= self.len {
            return None;
        }
        // SAFETY: `i < len` and the constructor guarantees an initialised `T`
        // there; the handle is bound by `&self`.
        Some(unsafe { handle_ref(NonNull::new_unchecked(self.ptr.as_ptr().add(i))) })
    }

    /// Exclusive handle to element `i`; `None` if out of range.
    ///
    /// Bound by `&mut self`, NOT by `'a`. Handing out a `T::Mut<'a>` here would
    /// let a caller keep it while calling [`get`](CSliceMut::get) — two handles
    /// to one object, which is what the exclusive handle exists to forbid.
    #[inline]
    #[must_use]
    pub fn get_mut(&mut self, i: usize) -> Option<T::Mut<'_>> {
        if i >= self.len {
            return None;
        }
        // SAFETY: `i < len`, initialised per the constructor, and the view's
        // own contract makes this the only handle to that element; the result
        // is bound by `&mut self`.
        Some(unsafe { handle_mut(NonNull::new_unchecked(self.ptr.as_ptr().add(i))) })
    }

    /// Exclusive handle to element `i` for the view's whole lifetime `'a`,
    /// giving the view up; `None` if out of range. The by-value counterpart
    /// of [`get_mut`](CSliceMut::get_mut), as indexing an owned `&'a mut [T]`
    /// yields a `&'a mut T`.
    #[inline]
    #[must_use]
    pub fn into_mut(self, i: usize) -> Option<T::Mut<'a>>
    where
        T: 'a,
    {
        if i >= self.len {
            return None;
        }
        // SAFETY: `i < len`, initialised and living for `'a` per the
        // constructor; the view is consumed, so this is the only route left to
        // the element.
        Some(unsafe { handle_mut(NonNull::new_unchecked(self.ptr.as_ptr().add(i))) })
    }

    /// Iterate the run as shared handles.
    #[inline]
    pub fn iter(&self) -> impl Iterator<Item = T::Ref<'_>> {
        let (ptr, len) = (self.ptr, self.len);
        // SAFETY: `i < len`, per the constructor's contract.
        (0..len).map(move |i| unsafe { handle_ref(NonNull::new_unchecked(ptr.as_ptr().add(i))) })
    }

    /// Iterate the run as exclusive handles.
    ///
    /// Every item borrows `&mut self`, so the whole run stays exclusively
    /// borrowed while any of them lives; the items are sound to hold at once
    /// because each addresses a distinct element, exactly as
    /// [`slice::iter_mut`](slice::iter_mut) does.
    #[inline]
    pub fn iter_mut<'s>(&'s mut self) -> impl Iterator<Item = T::Mut<'s>> + use<'s, T>
    where
        T: 's,
    {
        let (ptr, len) = (self.ptr, self.len);
        // SAFETY: `i` is distinct on every step, so no two items address the
        // same element; each is initialised per the constructor and bound by
        // the `&mut self` borrow.
        (0..len).map(move |i| unsafe { handle_mut(NonNull::new_unchecked(ptr.as_ptr().add(i))) })
    }

    /// Read-only pointer to the first element, for passing the run to C.
    ///
    /// `*const`, like [`<[T]>::as_ptr`](slice::as_ptr) and a shared handle's
    /// `as_ptr`: a C routine that is not const-correct takes `.cast_mut()`,
    /// which keeps the write visible at the call site.
    #[inline]
    #[must_use]
    pub fn as_ptr(&self) -> *const T::C {
        self.ptr.as_ptr().cast::<T::C>()
    }

    /// Writable pointer to the first element, for C calls that fill the run.
    #[inline]
    #[must_use]
    pub fn as_mut_ptr(&mut self) -> *mut T::C {
        self.ptr.as_ptr().cast::<T::C>()
    }
}

/// A run of plain values is read and written element-wise. See the
/// corresponding [`CSlice`] block for why [`CElem`] does not license a `&[T]`
/// over storage a C object owns.
impl<'a, T: CElem> CSliceMut<'a, T> {
    /// Copy element `i` out; `None` if out of range.
    #[inline]
    #[must_use]
    pub fn elem(&self, i: usize) -> Option<T>
    where
        T: Copy,
    {
        self.as_ref().elem(i)
    }

    /// Write element `i`; `false` — and nothing written — if out of range.
    #[inline]
    #[must_use]
    pub fn set_elem(&mut self, i: usize, v: T) -> bool {
        if i >= self.len {
            return false;
        }
        // SAFETY: `i < len`, the slot is initialised per the constructor, and
        // this view has exclusive access to it.
        unsafe { self.ptr.as_ptr().add(i).write(v) };
        true
    }

    /// Copy the whole run into `dst`. `false` if the lengths differ.
    #[inline]
    #[must_use]
    pub fn copy_to_slice(&self, dst: &mut [T]) -> bool
    where
        T: Copy,
    {
        self.as_ref().copy_to_slice(dst)
    }

    /// Overwrite the whole run from `src`. `false` — and nothing written — if
    /// the lengths differ.
    #[inline]
    #[must_use]
    pub fn copy_from_slice(&mut self, src: &[T]) -> bool
    where
        T: Copy,
    {
        if src.len() != self.len {
            return false;
        }
        // SAFETY: `len` slots at `ptr`, exclusive to this view, and `src` is a
        // live slice of the same length that cannot overlap them — it is a
        // Rust reference, which may not cover the C storage this views.
        unsafe { core::ptr::copy_nonoverlapping(src.as_ptr(), self.ptr.as_ptr(), self.len) };
        true
    }

    /// Read-only pointer to the first element, for passing the run to C.
    /// `*const`, as [`as_ptr`](Self::as_ptr).
    #[inline]
    #[must_use]
    pub fn as_elem_ptr(&self) -> *const T {
        self.ptr.as_ptr()
    }

    /// Writable pointer to the first element, for C calls that fill the run.
    #[inline]
    #[must_use]
    pub fn as_mut_elem_ptr(&mut self) -> *mut T {
        self.ptr.as_ptr()
    }
}
