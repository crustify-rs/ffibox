//! The handle and owner types: borrowed handle storage ([`CBorrowedPtr`]),
//! the sole owners ([`CBox`], [`CStrBox`], [`CVec`], [`CVal`]),
//! and the run views ([`CSlice`] / [`CSliceMut`]). The shared owners are in
//! [`shared`](crate::shared).
//! [docs/ownership.md](https://github.com/crustify-rs/ffibox/blob/main/docs/ownership.md)
//! says which to pick.
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
//! held by the owner — so an owner cannot be named without its destructor,
//! and one C type can have several. A type alias names the pair:
//! `pub type FooBox = CBox<Foo, FooFree>;`.
//!
//! ## Layout
//!
//! [`CBox`], [`CArc`](crate::CArc) and [`CStrBox`] are `#[repr(transparent)]`
//! over their pointer. Their policy is stateless and held in no bytes, so each
//! is the `T *` it stands in for, and `Option<_>` is the nullable one, `None`
//! being NULL, as Rust guarantees for `Option<NonNull<T>>`. They take a
//! `T *`'s place wherever C has one: an `extern "C"` parameter or return, a
//! `#[repr(C)]` struct field, an element of a `T **` buffer.
//!
//! A policy with state (an allocator context, a callback the release runs
//! through) is rejected by those three at compile time. It is held by
//! [`CBoxState`], `#[repr(C)]` over `{ptr, policy}`, which takes no `T *`'s
//! place: it crosses as [`into_raw`](CBoxState::into_raw) and comes back as
//! [`from_raw_with`](CBoxState::from_raw_with).
//!
//! [`CVal`] is `#[repr(C)]` with its value first, the size of its value with a
//! ZST policy. [`CVec`] stores pointer + length, the pointer NULL when empty,
//! as C's `{ T *ptr; size_t len; }`.

use core::ffi::c_char;
use core::fmt;
use core::marker::PhantomData;
use core::mem::{ManuallyDrop, MaybeUninit};
use core::ops::{Bound, RangeBounds};
use core::ptr::NonNull;

use crate::traits::{
    CAlloc, CAllocZeroed, CCell, CDispose, CDrop, CDupClone, CLenAlloc, CLenClone, CLenDrop, CNew,
    CPlainElem, CSliceElem, CZeroable,
};

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
/// conversion is a layout-preserving copy. Reached from outside only through
/// [`CCell::ref_from_ptr`] and its `void *` twin, both `unsafe`.
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

/// The pointer a shared handle of `T` holds; the inverse of [`handle_ref`].
#[inline]
pub(crate) fn ref_handle_ptr<'a, T: CCell + 'a>(h: &T::Ref<'a>) -> NonNull<T> {
    const {
        assert!(
            core::mem::size_of::<T::Ref<'a>>() == core::mem::size_of::<CBorrowedPtr<'a, T>>(),
            "CCell::Ref must be transparent over CBorrowedPtr",
        )
    };
    // SAFETY: `CCell`'s contract makes `T::Ref<'a>` transparent over
    // `CBorrowedPtr<'a, T>`, so `h` addresses a valid, aligned
    // `CBorrowedPtr<'a, T>` (sizes checked above), which is `Copy`.
    let raw = unsafe { *core::ptr::from_ref(h).cast::<CBorrowedPtr<'a, T>>() };
    raw.as_non_null()
}

/// The pointer an exclusive handle of `T` holds; the inverse of
/// [`handle_mut`]. Takes `&mut`, so the pointer keeps the handle's write
/// provenance.
#[inline]
pub(crate) fn mut_handle_ptr<'a, T: CCell + 'a>(h: &mut T::Mut<'a>) -> NonNull<T> {
    const {
        assert!(
            core::mem::size_of::<T::Mut<'a>>() == core::mem::size_of::<CBorrowedPtr<'a, T>>(),
            "CCell::Mut must be transparent over CBorrowedPtr",
        )
    };
    // SAFETY: as `ref_handle_ptr`, for `T::Mut<'a>`, which `CCell`'s contract
    // also makes transparent over `CBorrowedPtr<'a, T>`.
    let raw = unsafe { *core::ptr::from_mut(h).cast::<CBorrowedPtr<'a, T>>() };
    raw.as_non_null()
}

/// The pointer a locked handle of `T` holds. As [`mut_handle_ptr`], for
/// [`CGuarded::Locked`](crate::CGuarded::Locked).
#[inline]
pub(crate) fn locked_handle_ptr<'a, T: crate::CGuarded + 'a>(h: &mut T::Locked<'a>) -> NonNull<T> {
    const {
        assert!(
            core::mem::size_of::<T::Locked<'a>>() == core::mem::size_of::<CBorrowedPtr<'a, T>>(),
            "CGuarded::Locked must be transparent over CBorrowedPtr",
        )
    };
    // SAFETY: `CGuarded`'s contract makes `T::Locked<'a>` transparent over
    // `CBorrowedPtr<'a, T>`, so `h` addresses a valid, aligned
    // `CBorrowedPtr<'a, T>` (sizes checked above), which is `Copy`.
    let raw = unsafe { *core::ptr::from_mut(h).cast::<CBorrowedPtr<'a, T>>() };
    raw.as_non_null()
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

/// The policy slot of a `#[repr(transparent)]` owner: a policy of size zero,
/// held in no bytes.
///
/// A ZST has no bytes to store, so the slot keeps none: [`new`](Self::new)
/// takes the policy and forgets it, [`Deref`](core::ops::Deref) hands out a
/// `&D` at a dangling, well-aligned address, where a reference to a ZST is
/// valid, and `Drop` drops it with the owner. To the compiler the slot is a
/// `PhantomData<D>`, a 1-ZST whatever `D` is, which is what lets the owner be
/// transparent; its auto traits and drop check follow `D`, as a field of type
/// `D` would.
///
/// A policy with state cannot live in it, and every path to the policy
/// rejects one at compile time: filling the slot, reaching the policy, and
/// dropping it. An owner that never went through [`new`](Self::new), such as
/// one C passes to an `extern "C"` routine, still reaches its policy only
/// through the checked paths.
pub(crate) struct ZstPolicy<D>(PhantomData<D>);

impl<D> ZstPolicy<D> {
    /// Fails the build for a `D` with state.
    const STATELESS: () = assert!(
        core::mem::size_of::<D>() == 0,
        "ffibox: CBox, CArc and CStrBox hold a stateless (zero-sized) policy; \
         a policy with state needs CBoxState",
    );

    /// Hold `policy`, which must be a ZST.
    #[inline(always)]
    pub(crate) fn new(policy: D) -> Self {
        let () = Self::STATELESS;
        core::mem::forget(policy);
        Self(PhantomData)
    }

    /// The policy, moved out of the slot.
    #[inline(always)]
    pub(crate) fn into_inner(self) -> D {
        let () = Self::STATELESS;
        let _slot = ManuallyDrop::new(self);
        // SAFETY: `D` is a ZST (`STATELESS` asserts it), so reading one at a
        // dangling, well-aligned address reads no bytes. The slot owns the
        // value `new` forgot and is not dropped, so the value moves out once.
        unsafe { NonNull::<D>::dangling().as_ptr().read() }
    }
}

impl<D> core::ops::Deref for ZstPolicy<D> {
    type Target = D;

    #[inline(always)]
    fn deref(&self) -> &D {
        let () = Self::STATELESS;
        // SAFETY: as `into_inner`: a reference to a ZST is valid at any
        // dangling, well-aligned address, and the slot owns a value of `D`.
        unsafe { NonNull::<D>::dangling().as_ref() }
    }
}

impl<D: Clone> Clone for ZstPolicy<D> {
    #[inline]
    fn clone(&self) -> Self {
        Self::new(D::clone(self))
    }
}

impl<D> Drop for ZstPolicy<D> {
    #[inline]
    fn drop(&mut self) {
        let () = Self::STATELESS;
        // SAFETY: as `into_inner`; the slot's value is dropped once, here.
        unsafe { NonNull::<D>::dangling().as_ptr().drop_in_place() }
    }
}

// ---------------------------------------------------------------------------
// CBox<T, D> — the sole owner of a C-allocated object
// ---------------------------------------------------------------------------

/// The sole owner of a C-allocated `T`, released on drop by the policy `D` —
/// a `Box` whose destructor is a C routine.
///
/// `T` is a [`CCell`] layout type from [`define_ctype!`](crate::define_ctype):
/// every method requires it, so no `CBox` of anything else can be formed. A
/// single heap object crossing FFI is a C struct; a scalar or a buffer of
/// them is a [`CVec`], a C string a [`CStrBox`], and a Rust value is a `Box`.
/// The layout type unlocks the handles ([`as_ref`](Self::as_ref) /
/// [`as_mut`](Self::as_mut)) and the C-typed seam ([`from_c`](Self::from_c) /
/// [`into_c`](Self::into_c) / [`as_c_ptr`](Self::as_c_ptr)).
///
/// ```compile_fail,E0599
/// struct U32Free;
/// // SAFETY: never called; the box below must not type-check.
/// unsafe impl ffibox::CDrop<u32> for U32Free {
///     unsafe fn c_drop(&self, _: core::ptr::NonNull<u32>) {}
/// }
/// let _ = unsafe { ffibox::CBox::<u32, U32Free>::from_raw_with(core::ptr::null_mut(), U32Free) };
/// ```
///
/// **Sole** is the contract: `as_mut` hands out the exclusive handle from
/// `&mut self`, which is sound only if nothing else — another owner, or C —
/// uses the object meanwhile. A refcounted object fits while this is its only
/// reference (a fresh object, with its down-ref as the policy), but `CBox`
/// never shares it: `Clone` is a deep copy through [`CDupClone`], never an
/// `up_ref`.
///
/// `Send` / `Sync` on `Box`'s terms: `T: Send` / `T: Sync`, plus the policy's.
///
/// `#[repr(transparent)]` over the pointer, so `Option<CBox<T, D>>` is a
/// nullable `T *` (see the [module docs](self#layout)). The policy is
/// stateless; one with state fails to build, and is held by a [`CBoxState`]:
///
/// ```compile_fail,E0080
/// # use core::ptr::NonNull;
/// # use ffibox::{define_ctype, CBox, CDrop};
/// # mod ffi { #[repr(C)] pub struct foo_st { pub x: u32 } }
/// # define_ctype!(Foo, FooRef, FooMut, ffi::foo_st);
/// /// Frees into a pool: the policy holds the pool's handle.
/// struct PoolFree(usize);
/// // SAFETY: never called; the box below must not build.
/// unsafe impl CDrop<Foo> for PoolFree {
///     unsafe fn c_drop(&self, _: NonNull<Foo>) {}
/// }
/// let _ = unsafe { CBox::<Foo, PoolFree>::from_raw_with(core::ptr::null_mut(), PoolFree(1)) };
/// ```
#[repr(transparent)]
#[must_use = "dropping a CBox runs its teardown policy"]
pub struct CBox<T, D: CDrop<T>> {
    ptr: NonNull<T>,
    policy: ZstPolicy<D>,
}

impl<T: CCell, D: CDrop<T>> CBox<T, D> {
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
        NonNull::new(ptr).map(|ptr| Self {
            ptr,
            policy: ZstPolicy::new(policy),
        })
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
        unsafe {
            disarm(self, |o| {
                (o.ptr.as_ptr(), core::ptr::read(&o.policy).into_inner())
            })
        }
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
        (
            CBox {
                ptr,
                policy: ZstPolicy::new(policy),
            },
            old,
        )
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

    /// Adopt the object a C routine returns through an out-parameter
    /// (`int foo_open(foo **out, …)`), with the routine's own return value:
    /// the slot starts NULL, and is adopted under `D::default()` whatever the
    /// routine returns, so an object it hands over on failure is still freed.
    ///
    /// ```ignore
    /// // SAFETY: `foo_open` stores an owned `foo` or NULL in its slot.
    /// let (rc, foo) = unsafe { FooBox::from_c_out(|out| ffi::foo_open(out, path)) };
    /// if rc < 0 { return Err(Error::from(rc)); }
    /// ```
    ///
    /// # Safety
    ///
    /// `f` leaves NULL or an object [`from_raw`](Self::from_raw) may adopt in
    /// the slot.
    #[inline]
    pub unsafe fn from_c_out<R>(f: impl FnOnce(*mut *mut T::C) -> R) -> (R, Option<Self>)
    where
        D: Default,
    {
        let mut slot: *mut T::C = core::ptr::null_mut();
        let ret = f(&raw mut slot);
        // SAFETY: the caller vouches for what `f` left in the slot.
        (ret, unsafe { Self::from_c(slot) })
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

/// The safe constructors: each goes through the policy that will free the
/// object, so a box can only be built from its own allocator. They return
/// `None` when C reports failure, as [`from_raw`](Self::from_raw) does on
/// null, so translated C maps its `if (!p)` check onto `?`.
impl<T: CCell, D: CDrop<T>> CBox<T, D> {
    /// Construct through the policy's zero-argument C constructor
    /// ([`CNew`]) under `D::default()`; `None` on failure.
    #[inline]
    pub fn new() -> Option<Self>
    where
        D: CNew<T> + Default,
    {
        Self::new_with(D::default())
    }

    /// As [`new`](Self::new), under `policy`.
    #[inline]
    pub fn new_with(policy: D) -> Option<Self>
    where
        D: CNew<T>,
    {
        let ptr = policy.c_new()?;
        Some(Self {
            ptr,
            policy: ZstPolicy::new(policy),
        })
    }

    /// Allocate an all-zero `T` through the policy's zeroing allocator
    /// ([`CAllocZeroed`]) under `D::default()`; `None` on failure.
    #[inline]
    pub fn new_zeroed() -> Option<Self>
    where
        T: CZeroable,
        D: CAllocZeroed<T> + Default,
    {
        Self::new_zeroed_with(D::default())
    }

    /// As [`new_zeroed`](Self::new_zeroed), under `policy`.
    #[inline]
    pub fn new_zeroed_with(policy: D) -> Option<Self>
    where
        T: CZeroable,
        D: CAllocZeroed<T>,
    {
        let ptr = policy.c_alloc_zeroed()?;
        Some(Self {
            ptr,
            policy: ZstPolicy::new(policy),
        })
    }

    /// Allocate uninitialised storage through the policy's allocator
    /// ([`CAlloc`]) under `D::default()`; `None` on failure. C fills it in
    /// place through that box's `as_c_ptr`, then
    /// [`assume_init`](CBox::assume_init) promotes it.
    ///
    /// There is no safe `write(value)`: a C object is initialised where it
    /// lives, and a by-value `T` moved in would either be a zeroed one
    /// ([`new_zeroed`](Self::new_zeroed) builds that in place) or one owning
    /// resources a storage-only policy would leak.
    ///
    /// Dropped before it is filled, the box frees the storage through the
    /// policy's `CDrop<MaybeUninit<T>>`, which never reads it.
    #[inline]
    pub fn new_uninit() -> Option<CBox<MaybeUninit<T>, D>>
    where
        D: CAlloc<T> + Default,
    {
        Self::new_uninit_with(D::default())
    }

    /// As [`new_uninit`](Self::new_uninit), under `policy`.
    #[inline]
    pub fn new_uninit_with(policy: D) -> Option<CBox<MaybeUninit<T>, D>>
    where
        D: CAlloc<T>,
    {
        let ptr = policy.c_alloc()?;
        Some(CBox {
            ptr,
            policy: ZstPolicy::new(policy),
        })
    }
}

/// Storage not yet holding a `T` — a `Box<MaybeUninit<T>, A>`. No handles: the
/// accessors assume a valid object, so they wait for
/// [`assume_init`](Self::assume_init).
///
/// ```compile_fail,E0599
/// # use core::{mem::MaybeUninit, ptr::NonNull};
/// # use ffibox::{define_ctype, CAlloc, CBox, CDrop};
/// # mod ffi { #[repr(C)] pub struct foo_st { pub x: u32 } }
/// # define_ctype!(Foo, FooRef, FooMut, ffi::foo_st);
/// # struct Heap;
/// # unsafe impl CDrop<Foo> for Heap { unsafe fn c_drop(&self, _: NonNull<Foo>) {} }
/// # unsafe impl CDrop<MaybeUninit<Foo>> for Heap {
/// #     unsafe fn c_drop(&self, _: NonNull<MaybeUninit<Foo>>) {}
/// # }
/// # unsafe impl CAlloc<Foo> for Heap {
/// #     fn c_alloc(&self) -> Option<NonNull<MaybeUninit<Foo>>> { None }
/// # }
/// let b = CBox::<Foo, Heap>::new_uninit_with(Heap).unwrap();
/// let _ = b.as_ref(); // no getters on uninitialised storage
/// ```
impl<T: CCell, D: CAlloc<T>> CBox<MaybeUninit<T>, D> {
    /// Treat the storage as holding a `T`, after C filled it through
    /// [`as_c_ptr`](Self::as_c_ptr).
    ///
    /// # Safety
    ///
    /// The storage must hold a `T` that every safe operation on it accepts —
    /// the getters and setters, the safe wrappers taking a `T`, and the
    /// policy's [`c_drop`](CDrop::c_drop).
    #[inline]
    pub unsafe fn assume_init(self) -> CBox<T, D> {
        // SAFETY: each field is read out exactly once and `self` is not
        // dropped.
        let (ptr, policy) = unsafe { disarm(self, |o| (o.ptr, core::ptr::read(&o.policy))) };
        CBox {
            ptr: ptr.cast(),
            policy,
        }
    }

    /// The storage as the C type's pointer, for a C routine to fill in place.
    /// Ownership is retained.
    #[inline]
    #[must_use]
    pub fn as_c_ptr(&mut self) -> *mut T::C {
        self.ptr.as_ptr().cast()
    }
}

impl<T, D: CDrop<T>> Drop for CBox<T, D> {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: `self.ptr` is a live `T` we own; the policy releases it once.
        unsafe { self.policy.c_drop(self.ptr) }
    }
}

impl<T: CCell, D: CDupClone<T> + Clone> CBox<T, D> {
    /// Deep copy through [`CDupClone::c_dup`]; `None` if the C routine failed.
    ///
    /// [`Clone::clone`] aborts on that failure instead, because `Clone` is
    /// infallible. Use this where the C code checked the `*_dup` result:
    ///
    /// ```ignore
    /// let copy = key.try_clone().ok_or(Error::DupFailed)?;
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

impl<T: CCell, D: CDupClone<T> + Clone> Clone for CBox<T, D> {
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

// ---------------------------------------------------------------------------
// CBoxState<T, D> — the sole owner, under a policy with state
// ---------------------------------------------------------------------------

/// [`CBox`] under a policy with state: an allocator context
/// (`pool_free(pool, p)`), an arena, a callback the release runs through.
///
/// The policy is stored beside the pointer, so a `CBoxState` is `#[repr(C)]`
/// over `{ptr, policy}` and takes no `T *`'s place: across `extern "C"`, in a
/// `#[repr(C)]` field or in a `T **` buffer, it goes as
/// [`into_raw`](Self::into_raw) and comes back as
/// [`from_raw_with`](Self::from_raw_with).
///
/// It offers `CBox`'s core: adopting and surrendering, the handles, the
/// constructors that allocate through the policy, and `Clone`. A [`CBox`]
/// converts into one with `From`.
///
/// ```
/// use core::ptr::NonNull;
/// use ffibox::{define_ctype, CBoxState, CDrop};
///
/// mod ffi {
///     #[repr(C)]
///     pub struct foo_st { pub x: u32 }
///     #[repr(C)]
///     pub struct pool_st { pub live: usize }
///     // Stand-ins for a pool allocator's pair.
///     pub unsafe fn pool_alloc(pool: *mut pool_st) -> *mut foo_st {
///         unsafe { (*pool).live += 1 };
///         Box::into_raw(Box::new(foo_st { x: 0 }))
///     }
///     pub unsafe fn pool_free(pool: *mut pool_st, p: *mut foo_st) {
///         unsafe { (*pool).live -= 1 };
///         drop(unsafe { Box::from_raw(p) });
///     }
/// }
/// define_ctype!(Foo, FooRef, FooMut, ffi::foo_st);
///
/// /// Frees into the pool it names.
/// struct PoolFree(*mut ffi::pool_st);
/// // SAFETY: frees an object `pool_alloc` allocated from the same pool.
/// unsafe impl CDrop<Foo> for PoolFree {
///     unsafe fn c_drop(&self, p: NonNull<Foo>) {
///         unsafe { ffi::pool_free(self.0, p.as_ptr().cast()) }
///     }
/// }
///
/// let mut pool = ffi::pool_st { live: 0 };
/// let pool = &raw mut pool;
/// // SAFETY: a fresh object from `pool`, which outlives the box.
/// let foo = unsafe { CBoxState::from_c_with(ffi::pool_alloc(pool), PoolFree(pool)) }.unwrap();
/// assert_eq!(unsafe { (*pool).live }, 1);
/// drop(foo); // pool_free(pool, foo)
/// assert_eq!(unsafe { (*pool).live }, 0);
/// ```
///
/// `Send` / `Sync` on `Box`'s terms, plus the policy's.
#[repr(C)]
#[must_use = "dropping a CBoxState runs its teardown policy"]
pub struct CBoxState<T, D: CDrop<T>> {
    ptr: NonNull<T>,
    policy: D,
}

impl<T: CCell, D: CDrop<T>> CBoxState<T, D> {
    /// Take ownership of a raw pointer under `policy`; `None` if null.
    ///
    /// # Safety
    ///
    /// As [`CBox::from_raw`], with `policy` releasing it.
    #[inline]
    pub unsafe fn from_raw_with(ptr: *mut T, policy: D) -> Option<Self> {
        NonNull::new(ptr).map(|ptr| Self { ptr, policy })
    }

    /// [`from_raw_with`](Self::from_raw_with) from the C type's pointer.
    ///
    /// # Safety
    ///
    /// As [`CBox::from_raw`], with `policy` releasing it.
    #[inline]
    pub unsafe fn from_c_with(ptr: *mut T::C, policy: D) -> Option<Self> {
        // SAFETY: `T` is layout-compatible with `T::C` per `CCell`; the caller
        // upholds the rest.
        unsafe { Self::from_raw_with(ptr.cast(), policy) }
    }

    /// Raw pointer for passing to C. Ownership is retained.
    #[inline]
    #[must_use]
    pub fn as_ptr(&self) -> *mut T {
        self.ptr.as_ptr()
    }

    /// [`as_ptr`](Self::as_ptr) as the C type's pointer.
    #[inline]
    #[must_use]
    pub fn as_c_ptr(&self) -> *mut T::C {
        self.as_ptr().cast()
    }

    /// The policy that will release the object.
    #[inline]
    #[must_use]
    pub fn policy(&self) -> &D {
        &self.policy
    }

    /// Give up ownership without running the policy, which is dropped. The
    /// caller becomes responsible for releasing the object.
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

    /// [`into_raw`](Self::into_raw) as the C type's pointer.
    #[inline]
    #[must_use = "the returned pointer owns the object and must be freed"]
    pub fn into_c(self) -> *mut T::C {
        self.into_raw().cast()
    }

    /// Shared handle to the object, as [`CBox::as_ref`].
    #[inline]
    #[must_use]
    pub fn as_ref(&self) -> T::Ref<'_> {
        // SAFETY: `self.ptr` addresses a live `T` we own; the handle is bound
        // to this borrow.
        unsafe { handle_ref(self.ptr) }
    }

    /// Exclusive handle to the object, as [`CBox::as_mut`].
    #[inline]
    #[must_use]
    pub fn as_mut(&mut self) -> T::Mut<'_> {
        // SAFETY: as `as_ref`, from an exclusive borrow of the sole owner.
        unsafe { handle_mut(self.ptr) }
    }
}

/// The safe constructors, as [`CBox`]'s `*_with` ones.
impl<T: CCell, D: CDrop<T>> CBoxState<T, D> {
    /// Construct through the policy's C constructor ([`CNew`]); `None` on
    /// failure.
    #[inline]
    pub fn new_with(policy: D) -> Option<Self>
    where
        D: CNew<T>,
    {
        let ptr = policy.c_new()?;
        Some(Self { ptr, policy })
    }

    /// Allocate an all-zero `T` through the policy ([`CAllocZeroed`]); `None`
    /// on failure.
    #[inline]
    pub fn new_zeroed_with(policy: D) -> Option<Self>
    where
        T: CZeroable,
        D: CAllocZeroed<T>,
    {
        let ptr = policy.c_alloc_zeroed()?;
        Some(Self { ptr, policy })
    }

    /// Allocate uninitialised storage through the policy ([`CAlloc`]), as
    /// [`CBox::new_uninit`]; `None` on failure.
    #[inline]
    pub fn new_uninit_with(policy: D) -> Option<CBoxState<MaybeUninit<T>, D>>
    where
        D: CAlloc<T>,
    {
        let ptr = policy.c_alloc()?;
        Some(CBoxState { ptr, policy })
    }
}

impl<T: CCell, D: CAlloc<T>> CBoxState<MaybeUninit<T>, D> {
    /// Treat the storage as holding a `T`, as [`CBox::assume_init`].
    ///
    /// # Safety
    ///
    /// As [`CBox::assume_init`].
    #[inline]
    pub unsafe fn assume_init(self) -> CBoxState<T, D> {
        // SAFETY: each field is read out exactly once and `self` is not
        // dropped.
        let (ptr, policy) = unsafe { disarm(self, |o| (o.ptr, core::ptr::read(&o.policy))) };
        CBoxState {
            ptr: ptr.cast(),
            policy,
        }
    }

    /// The storage as the C type's pointer, for a C routine to fill in place.
    #[inline]
    #[must_use]
    pub fn as_c_ptr(&mut self) -> *mut T::C {
        self.ptr.as_ptr().cast()
    }
}

impl<T, D: CDrop<T>> Drop for CBoxState<T, D> {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: `self.ptr` is a live `T` we own; the policy releases it once.
        unsafe { self.policy.c_drop(self.ptr) }
    }
}

impl<T: CCell, D: CDupClone<T> + Clone> CBoxState<T, D> {
    /// Deep copy through [`CDupClone::c_dup`], as [`CBox::try_clone`].
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

impl<T: CCell, D: CDupClone<T> + Clone> Clone for CBoxState<T, D> {
    #[inline]
    fn clone(&self) -> Self {
        // Abort on a failed copy, as `CBox` does.
        match self.try_clone() {
            Some(b) => b,
            None => abort_process(),
        }
    }
}

impl<T, D: CDrop<T>> From<CBox<T, D>> for CBoxState<T, D> {
    /// Hold a box's object under the same policy, now stored.
    #[inline]
    fn from(b: CBox<T, D>) -> Self {
        // SAFETY: each field is read out exactly once and `b` is not dropped.
        let (ptr, policy) = unsafe { disarm(b, |o| (o.ptr, core::ptr::read(&o.policy))) };
        Self {
            ptr,
            policy: policy.into_inner(),
        }
    }
}

impl<T, D: CDrop<T>> fmt::Debug for CBoxState<T, D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("CBoxState")
            .field(&self.ptr.as_ptr())
            .finish()
    }
}

impl<T, D: CDrop<T>> fmt::Pointer for CBoxState<T, D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Pointer::fmt(&self.ptr.as_ptr(), f)
    }
}

// SAFETY: as `CBox`: the sole owner of its `T`, with the policy travelling
// with the pointer and running on whichever thread drops it.
unsafe impl<T: Send, D: CDrop<T> + Send> Send for CBoxState<T, D> {}
// SAFETY: as above; `&CBoxState<T, D>` hands out `T::Ref<'_>` and `&D`.
unsafe impl<T: Sync, D: CDrop<T> + Sync> Sync for CBoxState<T, D> {}

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
///
/// `#[repr(transparent)]` over the pointer, as [`CBox`] is, so
/// `Option<CStrBox<D>>` is a nullable `char *`, and the policy is stateless.
// TODO(CStrBoxState): a string under a policy with state, as `CBoxState` is
// for `CBox`, once a campaign frees strings through an allocator context.
#[repr(transparent)]
#[must_use = "dropping a CStrBox runs its teardown policy"]
pub struct CStrBox<D: CDrop<c_char>> {
    ptr: NonNull<c_char>,
    policy: ZstPolicy<D>,
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
        NonNull::new(ptr).map(|ptr| Self {
            ptr,
            policy: ZstPolicy::new(policy),
        })
    }

    /// Adopt the string a C routine returns through an out-parameter
    /// (`int get_name(obj *o, char **out)`), with the routine's own return
    /// value; as [`CBox::from_c_out`].
    ///
    /// # Safety
    ///
    /// `f` leaves NULL or a string [`from_raw`](Self::from_raw) may adopt in
    /// the slot.
    #[inline]
    pub unsafe fn from_raw_out<R>(f: impl FnOnce(*mut *mut c_char) -> R) -> (R, Option<Self>)
    where
        D: Default,
    {
        let mut slot: *mut c_char = core::ptr::null_mut();
        let ret = f(&raw mut slot);
        // SAFETY: the caller vouches for what `f` left in the slot.
        (ret, unsafe { Self::from_raw(slot) })
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
        unsafe {
            disarm(self, |o| {
                (o.ptr.as_ptr(), core::ptr::read(&o.policy).into_inner())
            })
        }
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

impl<D: CDupClone<c_char>> CStrBox<D> {
    /// Copy a Rust-side string into the policy's allocator (`strdup`) under
    /// `D::default()`; `None` if the C copy failed. The way Rust hands C a
    /// string it will free.
    #[inline]
    pub fn from_c_str(s: &core::ffi::CStr) -> Option<Self>
    where
        D: Default,
    {
        Self::from_c_str_with(s, D::default())
    }

    /// As [`from_c_str`](Self::from_c_str), under `policy`.
    #[inline]
    pub fn from_c_str_with(s: &core::ffi::CStr, policy: D) -> Option<Self> {
        let src = NonNull::from(s).cast::<c_char>();
        // SAFETY: `src` is a live NUL-terminated string for the call, which
        // `CDupClone<c_char>` lets `c_dup` copy wherever it lives, reading
        // it only; a `Some` is a fresh copy the policy releases.
        let ptr = unsafe { policy.c_dup(src) }?;
        Some(Self {
            ptr,
            policy: ZstPolicy::new(policy),
        })
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

impl<D: CDrop<c_char>> core::ops::Deref for CStrBox<D> {
    type Target = core::ffi::CStr;

    /// The string, as `CString` derefs to it: [`as_c_str`](Self::as_c_str).
    #[inline]
    fn deref(&self) -> &core::ffi::CStr {
        self.as_c_str()
    }
}

impl<D: CDrop<c_char>> AsRef<core::ffi::CStr> for CStrBox<D> {
    #[inline]
    fn as_ref(&self) -> &core::ffi::CStr {
        self.as_c_str()
    }
}

impl<D: CDrop<c_char>> core::borrow::Borrow<core::ffi::CStr> for CStrBox<D> {
    #[inline]
    fn borrow(&self) -> &core::ffi::CStr {
        self.as_c_str()
    }
}

/// Compared by content, as `CString` is, whatever the policies.
impl<D: CDrop<c_char>, E: CDrop<c_char>> PartialEq<CStrBox<E>> for CStrBox<D> {
    #[inline]
    fn eq(&self, other: &CStrBox<E>) -> bool {
        self.as_c_str() == other.as_c_str()
    }
}
impl<D: CDrop<c_char>> Eq for CStrBox<D> {}

impl<D: CDrop<c_char>> PartialEq<core::ffi::CStr> for CStrBox<D> {
    #[inline]
    fn eq(&self, other: &core::ffi::CStr) -> bool {
        self.as_c_str() == other
    }
}

impl<D: CDrop<c_char>> PartialEq<&core::ffi::CStr> for CStrBox<D> {
    #[inline]
    fn eq(&self, other: &&core::ffi::CStr) -> bool {
        self.as_c_str() == *other
    }
}

impl<D: CDrop<c_char>> PartialOrd for CStrBox<D> {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl<D: CDrop<c_char>> Ord for CStrBox<D> {
    #[inline]
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.as_c_str().cmp(other.as_c_str())
    }
}

/// Hashed by content, consistently with `Borrow<CStr>`.
impl<D: CDrop<c_char>> core::hash::Hash for CStrBox<D> {
    #[inline]
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.as_c_str().hash(state);
    }
}

/// A copy into Rust's allocator, releasing the C string through its policy.
#[cfg(feature = "alloc")]
impl<D: CDrop<c_char>> From<CStrBox<D>> for alloc::ffi::CString {
    #[inline]
    fn from(s: CStrBox<D>) -> Self {
        s.as_c_str().into()
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
/// Plain elements and owners ([`CSliceElem`]) read out as a real `&[T]`,
/// which is sound because the buffer is owned exclusively. Wrapped C objects
/// come out as handles through [`as_handles`](Self::as_handles), since
/// `&[Foo]` would be a reference covering them.
///
/// **Dropping a `CVec` drops its elements**, as dropping a `Vec` does, then
/// frees the buffer. Which elements own something is their type's to say: a
/// `CVec<Option<CBox<Foo, FooFree>>, S>` is a `foo **` whose every non-null
/// slot `foo_free` releases, a `CVec<*mut ffi::foo, S>` one whose slots own
/// nothing, borrowed from elsewhere. The policy frees the buffer alone (see
/// [`CLenDrop`]), so an array C frees with its elements by one routine
/// (`foo_array_free(arr, n)`) is a buffer of owners under the array
/// allocator's policy, never bare pointers under that routine: bare pointers
/// are [`CPlainElem`], which safe code may overwrite through the `&mut [T]`
/// the `CVec` hands out, with a pointer the routine then frees. Where the
/// routine does more than free each element and then the array, the array
/// is wrapped by hand.
///
/// **Do not drop the elements a second time.** A wrapper that holds a `CVec`
/// leaves the elements to it: a `Drop` impl of its own that drops them
/// (`ptr::drop_in_place(v.as_mut_slice())`), or a policy whose `c_drop_len`
/// frees them, releases each one twice. Up to ffibox 0.1.1 a `CVec` freed the
/// buffer alone, and code written for that needed such a `Drop`; it still
/// compiles, so the
/// [CHANGELOG](https://github.com/crustify-rs/ffibox/blob/main/CHANGELOG.md)
/// says how to migrate it.
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
    /// - Elements that own what they point at (an owner such as
    ///   `Option<CBox<Foo, P>>`) are adopted with the buffer: each non-null
    ///   one must be an allocation of its own that its policy releases. A
    ///   pointer repeated in two slots is freed twice.
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

impl<T: CSliceElem, S: CLenDrop> CVec<T, S> {
    /// The elements as a slice.
    #[inline]
    #[must_use]
    pub fn as_slice(&self) -> &[T] {
        // SAFETY: `len` initialised elements at `data()` per `from_raw_parts`
        // (none, at an aligned dangling pointer, when NULL), Rust values by
        // `T: CSliceElem`, owned exclusively; bound by `&self`.
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

impl<T, S: CLenDrop> CVec<MaybeUninit<T>, S> {
    /// The buffer as initialized elements, once the caller has filled it —
    /// like `Box<[MaybeUninit<T>]>::assume_init`. The allocation, its length
    /// and the policy carry over unchanged; an empty NULL buffer stays one.
    ///
    /// A buffer from a non-zeroing allocator starts as `MaybeUninit<T>`, since
    /// uninitialized memory is not a valid `T` — not even a `u8`:
    ///
    /// ```ignore
    /// let mut buf: CVec<MaybeUninit<u8>, AvFree> = alloc_bytes(n)?;
    /// for b in buf.as_mut_slice() {
    ///     b.write(0xff);
    /// }
    /// // SAFETY: every element was written above.
    /// let buf: CVec<u8, AvFree> = unsafe { buf.assume_init() };
    /// ```
    ///
    /// # Safety
    ///
    /// Every one of the `len` elements must be initialized to a valid `T`.
    #[inline]
    pub unsafe fn assume_init(self) -> CVec<T, S> {
        let (ptr, len, policy) = self.into_raw_parts_with();
        // `MaybeUninit<T>` has `T`'s size and alignment, so the cast keeps the
        // byte length the policy frees with, and NULL stays NULL.
        CVec {
            ptr: NonNull::new(ptr.cast::<T>()),
            len,
            policy,
        }
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
        let Some(ptr) = self.ptr else { return };
        // Frees the buffer however the element drops end, a panic included.
        let _free = FreeBuffer {
            ptr: ptr.as_ptr().cast(),
            byte_len: self.byte_len(),
            policy: &self.policy,
        };
        if core::mem::needs_drop::<T>() {
            // SAFETY: `len` initialised elements this buffer owns, dropped
            // once here; nothing reads them afterwards.
            unsafe {
                core::ptr::drop_in_place(core::ptr::slice_from_raw_parts_mut(
                    ptr.as_ptr(),
                    self.len,
                ))
            }
        }
    }
}

/// Frees a buffer through its policy when dropped: the last step of a
/// [`CVec`]'s drop, and the cleanup of a constructor that fails or panics
/// part-way.
struct FreeBuffer<'a, S: CLenDrop> {
    ptr: *mut u8,
    byte_len: usize,
    policy: &'a S,
}

impl<S: CLenDrop> Drop for FreeBuffer<'_, S> {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: the creator hands over a live `byte_len`-byte allocation of
        // the policy's allocator, whose elements are dropped or were never
        // written.
        unsafe { self.policy.c_drop_len(self.ptr, self.byte_len) }
    }
}

impl<T, S: CLenAlloc> CVec<T, S> {
    /// The byte length of `len` elements, checked: `None` on overflow. Rejects
    /// zero-sized and over-aligned elements at compile time.
    #[inline]
    fn alloc_len(len: usize) -> Option<usize> {
        const {
            assert!(
                core::mem::size_of::<T>() != 0,
                "zero-sized elements need no buffer"
            );
            assert!(
                core::mem::align_of::<T>() <= <S as CLenAlloc>::ALIGN,
                "the policy's allocator cannot align T"
            );
        }
        len.checked_mul(core::mem::size_of::<T>())
    }

    /// A buffer of `len` elements, each `f(i)` in order, from the policy's
    /// allocator under `S::default()` — the way Rust builds an array C will
    /// free, as [`core::array::from_fn`] builds one Rust keeps. `None` if the
    /// allocation failed; a `len` of 0 is the empty, NULL buffer.
    ///
    /// The elements may own resources: a `char **` of owned strings is a
    /// `CVec<Option<CStrBox<LibFree>>, LibFree>`. If `f` panics, the elements
    /// written so far are dropped and the buffer freed.
    #[inline]
    pub fn from_fn(len: usize, f: impl FnMut(usize) -> T) -> Option<Self>
    where
        S: Default,
    {
        Self::from_fn_with(len, S::default(), f)
    }

    /// As [`from_fn`](Self::from_fn), under `policy`.
    pub fn from_fn_with(len: usize, policy: S, mut f: impl FnMut(usize) -> T) -> Option<Self> {
        let byte_len = Self::alloc_len(len)?;
        if len == 0 {
            return Some(Self::empty_with(policy));
        }
        let base = policy.c_alloc_len(byte_len)?.cast::<T>();

        /// Drops the elements written so far, then frees the buffer, unless
        /// disarmed by a complete fill.
        struct Filling<'a, T, S: CLenDrop> {
            base: NonNull<T>,
            written: usize,
            free: ManuallyDrop<FreeBuffer<'a, S>>,
        }
        impl<T, S: CLenDrop> Drop for Filling<'_, T, S> {
            fn drop(&mut self) {
                // SAFETY: `written` elements at `base` are initialised and
                // owned by nothing else; the buffer is freed once, after.
                unsafe {
                    core::ptr::drop_in_place(core::ptr::slice_from_raw_parts_mut(
                        self.base.as_ptr(),
                        self.written,
                    ));
                    ManuallyDrop::drop(&mut self.free);
                }
            }
        }

        let mut fill = Filling {
            base,
            written: 0,
            free: ManuallyDrop::new(FreeBuffer {
                ptr: base.as_ptr().cast(),
                byte_len,
                policy: &policy,
            }),
        };
        while fill.written < len {
            let value = f(fill.written);
            // SAFETY: `written < len`, within the `byte_len`-byte allocation,
            // aligned for `T` by `S::ALIGN >= align_of::<T>()`; the slot is
            // uninitialised, so nothing is overwritten.
            unsafe { base.as_ptr().add(fill.written).write(value) };
            fill.written += 1;
        }
        core::mem::forget(fill);
        Some(Self {
            ptr: Some(base),
            len,
            policy,
        })
    }

    /// A buffer of `len` uninitialised elements from the policy's allocator,
    /// under `S::default()`, for C or Rust to fill before
    /// [`assume_init`](CVec::assume_init). `None` if the allocation failed; a
    /// `len` of 0 is the empty, NULL buffer.
    #[inline]
    pub fn new_uninit(len: usize) -> Option<CVec<MaybeUninit<T>, S>>
    where
        S: Default,
    {
        Self::new_uninit_with(len, S::default())
    }

    /// As [`new_uninit`](Self::new_uninit), under `policy`.
    #[inline]
    pub fn new_uninit_with(len: usize, policy: S) -> Option<CVec<MaybeUninit<T>, S>> {
        let byte_len = Self::alloc_len(len)?;
        if len == 0 {
            return Some(CVec::empty_with(policy));
        }
        let base = policy.c_alloc_len(byte_len)?.cast::<MaybeUninit<T>>();
        Some(CVec {
            ptr: Some(base),
            len,
            policy,
        })
    }
}

impl<T: Copy, S: CLenClone + Clone> CVec<T, S> {
    /// Byte copy into a fresh allocation through the policy; `None` if the C
    /// copy failed. A NULL buffer clones to another, without calling C.
    ///
    /// The copy is only as aligned as [`S::ALIGN`](CLenClone::ALIGN) vouches
    /// for, so element types aligned beyond it are rejected at compile time,
    /// as by [`from_slice`](Self::from_slice) — even though the source buffer,
    /// adopted through [`from_raw_parts`](Self::from_raw_parts), was aligned:
    ///
    /// ```compile_fail,E0080
    /// # use core::ptr::NonNull;
    /// # #[derive(Clone)]
    /// # struct Bytes;
    /// # unsafe impl ffibox::CLenDrop for Bytes { unsafe fn c_drop_len(&self, _: *mut u8, _: usize) {} }
    /// # unsafe impl ffibox::CLenClone for Bytes {
    /// #     unsafe fn c_clone_len(&self, _: *mut u8, _: usize) -> Option<NonNull<u8>> { None }
    /// # }
    /// // `Bytes` vouches for byte alignment only, so a `u32` copy is refused.
    /// fn copy(v: &ffibox::CVec<u32, Bytes>) -> Option<ffibox::CVec<u32, Bytes>> {
    ///     v.try_clone()
    /// }
    /// # let _ = copy(&ffibox::CVec::empty_with(Bytes));
    /// ```
    #[inline]
    pub fn try_clone(&self) -> Option<Self> {
        const {
            assert!(
                core::mem::align_of::<T>() <= S::ALIGN,
                "the policy's allocator cannot align T"
            );
        }
        let Some(src) = self.ptr else {
            return Some(Self::empty_with(self.policy.clone()));
        };
        // SAFETY: `byte_len()` live bytes at `src`; per `CLenClone` a `Some`
        // is a fresh copy the policy releases, aligned to
        // `S::ALIGN >= align_of::<T>()`.
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

impl<T: Copy, S: CLenClone> CVec<T, S> {
    /// Copy `src` into a fresh allocation of the policy's allocator under
    /// `S::default()`; `None` if the copy failed. An empty `src` gives an
    /// empty buffer, without calling C.
    ///
    /// The way Rust hands C a buffer it will free: `CVec::<u8, LibFree>::from_slice(b)`
    /// is `lib_memdup(b, len)`. Element types aligned beyond
    /// [`S::ALIGN`](CLenClone::ALIGN), and zero-sized ones, are rejected at
    /// compile time.
    ///
    /// ```compile_fail,E0080
    /// # use core::ptr::NonNull;
    /// # struct Bytes;
    /// # unsafe impl ffibox::CLenDrop for Bytes { unsafe fn c_drop_len(&self, _: *mut u8, _: usize) {} }
    /// # unsafe impl ffibox::CLenClone for Bytes {
    /// #     unsafe fn c_clone_len(&self, _: *mut u8, _: usize) -> Option<NonNull<u8>> { None }
    /// # }
    /// // `Bytes` vouches for byte alignment only, so a `u32` buffer is refused.
    /// let _ = ffibox::CVec::<u32, Bytes>::from_slice_with(&[1, 2], Bytes);
    /// ```
    ///
    /// ```compile_fail,E0080
    /// # use core::ptr::NonNull;
    /// # struct Bytes;
    /// # unsafe impl ffibox::CLenDrop for Bytes { unsafe fn c_drop_len(&self, _: *mut u8, _: usize) {} }
    /// # unsafe impl ffibox::CLenClone for Bytes {
    /// #     unsafe fn c_clone_len(&self, _: *mut u8, _: usize) -> Option<NonNull<u8>> { None }
    /// # }
    /// // Zero-sized elements have no bytes to copy.
    /// let _ = ffibox::CVec::<(), Bytes>::from_slice_with(&[(), ()], Bytes);
    /// ```
    #[inline]
    pub fn from_slice(src: &[T]) -> Option<Self>
    where
        S: Default,
    {
        Self::from_slice_with(src, S::default())
    }

    /// As [`from_slice`](Self::from_slice), under `policy`.
    #[inline]
    pub fn from_slice_with(src: &[T], policy: S) -> Option<Self> {
        // SAFETY: `src` is `src.len()` initialised, readable `T` for the call.
        unsafe { copy_run(src.as_ptr(), src.len(), policy) }
    }
}

/// Byte-copy `len` elements at `src` into a fresh `CVec` through `policy`.
///
/// # Safety
///
/// `src` must address `len` initialised `T`, readable for the call.
#[inline]
unsafe fn copy_run<T: Copy, S: CLenClone>(
    src: *const T,
    len: usize,
    policy: S,
) -> Option<CVec<T, S>> {
    const {
        assert!(
            core::mem::size_of::<T>() != 0,
            "zero-sized elements have no bytes to copy"
        );
        assert!(
            core::mem::align_of::<T>() <= S::ALIGN,
            "the policy's allocator cannot align T"
        );
    }
    if len == 0 {
        return Some(CVec::empty_with(policy));
    }
    let byte_len = len.checked_mul(core::mem::size_of::<T>())?;
    // SAFETY: the caller guarantees `byte_len` readable bytes at `src`, which
    // `c_clone_len` only reads. A `Some` is a fresh allocation of `byte_len`
    // bytes the policy releases, aligned to `S::ALIGN >= align_of::<T>()`,
    // holding a byte copy of `len` valid `T: Copy` values.
    let ptr = unsafe { policy.c_clone_len(src.cast_mut().cast(), byte_len) }?;
    Some(CVec {
        ptr: Some(ptr.cast()),
        len,
        policy,
    })
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

/// The elements, as `Vec` derefs to a slice: plain data and owners only.
impl<T: CSliceElem, S: CLenDrop> core::ops::Deref for CVec<T, S> {
    type Target = [T];

    #[inline]
    fn deref(&self) -> &[T] {
        self.as_slice()
    }
}

impl<T: CSliceElem, S: CLenDrop> core::ops::DerefMut for CVec<T, S> {
    #[inline]
    fn deref_mut(&mut self) -> &mut [T] {
        self.as_mut_slice()
    }
}

impl<T: CSliceElem, S: CLenDrop> AsRef<[T]> for CVec<T, S> {
    #[inline]
    fn as_ref(&self) -> &[T] {
        self.as_slice()
    }
}

impl<T: CSliceElem, S: CLenDrop> AsMut<[T]> for CVec<T, S> {
    #[inline]
    fn as_mut(&mut self) -> &mut [T] {
        self.as_mut_slice()
    }
}

impl<'a, T: CSliceElem, S: CLenDrop> IntoIterator for &'a CVec<T, S> {
    type Item = &'a T;
    type IntoIter = core::slice::Iter<'a, T>;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.as_slice().iter()
    }
}

impl<'a, T: CSliceElem, S: CLenDrop> IntoIterator for &'a mut CVec<T, S> {
    type Item = &'a mut T;
    type IntoIter = core::slice::IterMut<'a, T>;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.as_mut_slice().iter_mut()
    }
}

/// Compared element-wise, as `Vec` is, whatever the policies.
impl<T: CSliceElem + PartialEq, S: CLenDrop, S2: CLenDrop> PartialEq<CVec<T, S2>> for CVec<T, S> {
    #[inline]
    fn eq(&self, other: &CVec<T, S2>) -> bool {
        self.as_slice() == other.as_slice()
    }
}
impl<T: CSliceElem + Eq, S: CLenDrop> Eq for CVec<T, S> {}

impl<T: CSliceElem + PartialEq, S: CLenDrop> PartialEq<[T]> for CVec<T, S> {
    #[inline]
    fn eq(&self, other: &[T]) -> bool {
        self.as_slice() == other
    }
}

impl<T: CSliceElem + core::hash::Hash, S: CLenDrop> core::hash::Hash for CVec<T, S> {
    #[inline]
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.as_slice().hash(state);
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
/// define_ctype!(Params, ParamsRef, ParamsMut, ffi::params_st);
/// #[derive(Clone, Copy, Debug, Default)]
/// pub struct ParamsUninit;
/// impl_cdispose!(ParamsUninit, Params, ffi::params_uninit);
/// // SAFETY: zeroed params are the empty set, which every routine and
/// // `params_uninit` accept.
/// unsafe impl CZeroable for Params {}
///
/// let mut params: CVal<Params, ParamsUninit> = CVal::zeroed();
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
    ///
    /// # Safety
    ///
    /// `value` must own exclusively whatever its fields point at, and `D`
    /// must be the routine that releases exactly that: a value C just
    /// filled, or one [`into_inner`](Self::into_inner) gave back. A bitwise
    /// copy of a struct someone else owns (a field read out of a C parent,
    /// a duplicate of another value) is not one: disposing it frees what
    /// its owner still uses. A zeroed value owns nothing; build it with the
    /// safe [`zeroed`](Self::zeroed).
    ///
    /// ```compile_fail,E0133
    /// # use ffibox::{define_ctype, impl_cdispose, CVal, CZeroable};
    /// # mod ffi { #[repr(C)] pub struct buf_st { pub ptr: *mut u8 } pub unsafe fn buf_dispose(_: *mut buf_st) {} }
    /// # define_ctype!(Buf, BufRef, BufMut, ffi::buf_st);
    /// # #[derive(Default)] struct BufDispose;
    /// # impl_cdispose!(BufDispose, Buf, ffi::buf_dispose);
    /// # unsafe impl CZeroable for Buf {}
    /// let _ = CVal::<Buf, BufDispose>::new(Buf::zeroed()); // adopting needs `unsafe`
    /// ```
    #[inline]
    pub unsafe fn new(value: T) -> Self
    where
        D: Default,
    {
        // SAFETY: the caller upholds this function's contract.
        unsafe { Self::new_with(value, D::default()) }
    }

    /// Take ownership of a value under `policy`.
    ///
    /// # Safety
    ///
    /// As [`new`](Self::new), with `policy` the routine that releases it.
    #[inline]
    pub unsafe fn new_with(value: T, policy: D) -> Self {
        Self { value, policy }
    }

    /// An all-zero value; dropping the result disposes it.
    ///
    /// Safe: zero bytes own nothing, and [`CZeroable`] and [`CDispose`]
    /// each require that the teardown accepts them.
    #[inline]
    pub fn zeroed() -> Self
    where
        T: CZeroable,
        D: Default,
    {
        Self::zeroed_with(D::default())
    }

    /// An all-zero value under `policy`.
    #[inline]
    pub fn zeroed_with(policy: D) -> Self
    where
        T: CZeroable,
    {
        // SAFETY: an all-zero `T` holds null pointers and owns nothing, and
        // every policy paired with a `CZeroable` type accepts it (both
        // traits' contracts).
        unsafe { Self::new_with(T::zeroed(), policy) }
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
    ///
    /// Safe: the `CVal` is consumed, so the value is the only owner of its
    /// resources, and a bare `T` has no `Drop` — dropping it leaks, as
    /// [`mem::forget`](core::mem::forget) does. Handing it back to an owner
    /// is [`new`](Self::new)'s caller's to justify.
    #[inline]
    #[must_use = "the value still owns its resources; dropping it leaks them"]
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

// The owners are Rust values: a reference to one covers its pointer (or, for
// a `CVal`, a value Rust holds), never C memory, so a `CVec` of them hands
// out a real slice.
// SAFETY: a `CBox` is a pointer and its policy; the object behind it is
// reached only through handles.
unsafe impl<T, D: CDrop<T>> CSliceElem for CBox<T, D> {}
// SAFETY: as above; `None` is a NULL slot.
unsafe impl<T, D: CDrop<T>> CSliceElem for Option<CBox<T, D>> {}
// SAFETY: a `CStrBox` is a pointer and its policy; its views are read-only.
unsafe impl<D: CDrop<c_char>> CSliceElem for CStrBox<D> {}
// SAFETY: as above.
unsafe impl<D: CDrop<c_char>> CSliceElem for Option<CStrBox<D>> {}
// SAFETY: a `CVal` in a buffer the `CVec` owns is Rust-owned storage, as a
// `CVal` local is; its handles are reached through `as_ref` / `as_mut`.
unsafe impl<T: CCell, D: CDispose<T>> CSliceElem for CVal<T, D> {}

// ===========================================================================
// Run views — borrowed runs of contiguous elements, never a `&[T]` over
// memory C may write
// ===========================================================================

// ---------------------------------------------------------------------------
// CSlice<'a, T> — a borrowed run of elements
// ---------------------------------------------------------------------------

/// A borrowed run of `len` contiguous elements: wrapped C objects, yielded as
/// handles, or plain values, read out as copies.
///
/// The slice analogue of a `Ref` handle, and for the same reason: `&[T]` over
/// memory C may write would be a reference covering it, asserting `noalias` /
/// `readonly` / validity. A `CSlice` is a pointer and a count, so it asserts
/// nothing; [`get`](CSlice::get) and [`iter`](CSlice::iter) hand out
/// per-element handles, and [`elem`](CSlice::elem) and
/// [`elems`](CSlice::elems) copies of [`CPlainElem`] values.
///
/// Reached with [`CVec::as_handles`](crate::CVec::as_handles), or built over a
/// run C owns with [`from_raw_parts`](CSlice::from_raw_parts). A `CVec` of
/// plain values or owners takes [`CVec::as_slice`](crate::CVec::as_slice)
/// instead, which is a real `&[T]`, sound because the `CVec` owns its buffer.
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

/// A run of `Copy` elements — plain values or wrapped C objects whose bytes
/// own nothing — copies into an owner of its own, as a `CVec` of them clones.
impl<'a, T: Copy> CSlice<'a, T> {
    /// Copy the run into a fresh buffer of `S`'s allocator, under
    /// `S::default()`; `None` if the copy failed.
    ///
    /// The bytes are read straight from the C object, never through a `&[T]`,
    /// so this is sound while C keeps its pointer to the run. `T: Copy` is
    /// what makes a byte copy a whole value, as it does for
    /// [`CVec::clone`](crate::CVec::clone): a wrapped C object that owns
    /// nothing (a `{ num, den }` pair) qualifies as well as a plain integer.
    #[inline]
    pub fn to_cvec<S: CLenClone + Default>(&self) -> Option<CVec<T, S>> {
        self.to_cvec_with(S::default())
    }

    /// As [`to_cvec`](Self::to_cvec), under `policy`.
    #[inline]
    pub fn to_cvec_with<S: CLenClone>(&self, policy: S) -> Option<CVec<T, S>> {
        // SAFETY: a `CSlice` addresses `len` initialised `T` live for `'a`,
        // which `copy_run` only reads; `T: Copy` makes the byte copy `len`
        // whole values.
        unsafe { copy_run(self.ptr.as_ptr(), self.len, policy) }
    }
}

/// A run of plain values is read out element-wise, still without a `&[T]`.
///
/// [`CPlainElem`] is what licenses [`CVec::as_slice`](crate::CVec::as_slice) to hand
/// out a real `&[T]`, and it is not enough here — it answers *is every bit
/// pattern a valid `T`*, while a `&[T]` also asserts `noalias` and `readonly`
/// over the whole run for the whole borrow. `CVec` earns those by owning its
/// buffer exclusively. A run inside a C object does not: the library keeps the
/// pointer and may write through it, and no Rust lifetime constrains that. So
/// the element type is not what decides between `&[T]` and a `CSlice` — the
/// owner is, and who may write the run.
///
/// A run whose C contract fixes it for the object's life (content read once,
/// never written, freed with the object), which nothing reachable through the
/// shared handle writes, is the exception: the wrapper lends it as a
/// `&'a [T]` from `slice::from_raw_parts`, tied to that handle, as it lends a
/// NUL string as a `&CStr`. The conditions are listed under *Choosing a borrow*
/// in [docs/borrows.md](https://github.com/crustify-rs/ffibox/blob/main/docs/borrows.md#choosing-a-borrow).
/// Any other run is a `CSlice`.
impl<'a, T: CPlainElem> CSlice<'a, T> {
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
        // there, and `T: CPlainElem` makes every bit pattern a valid one. The read
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
/// corresponding [`CSlice`] block for why [`CPlainElem`] does not license a `&[T]`
/// over storage a C object owns.
impl<'a, T: CPlainElem> CSliceMut<'a, T> {
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
