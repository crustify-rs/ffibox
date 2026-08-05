//! Owning smart pointers: RAII handles that run a lifecycle trait's teardown on
//! `Drop`. [`CBox<T>`] for C-allocated objects, [`CVec<T, S>`] for length-aware
//! buffers, plus the by-value, type-erased, string, uninit-phase and
//! stateful-teardown variants below.
//!
//! There is deliberately **no separate refcounted owner**. Shared ownership is
//! a [`CBox<T>`] whose [`CDropped::c_drop`] is the down-ref and whose
//! [`CCloned::c_clone`] is the `up_ref` — the mechanism is chosen when you
//! register the C routines, not by the handle type. Neither flavour exposes
//! `&mut T` (mutation goes through the raw pointer and [`CCell`]'s interior
//! mutability), so an aliased and a sole-owner handle need no type-level
//! distinction. [`CBox::try_clone`] mirrors C's recoverable failure with
//! `None`; [`Clone::clone`] aborts on that essentially unreachable case.
//!
//! ## Layout compatibility
//!
//! [`CBox<T>`] is `#[repr(transparent)]` over [`NonNull<T>`], layout-compatible
//! with `*mut T` including the `Option` niche, so it substitutes for a raw
//! pointer field in a `#[repr(C)]` struct. [`CVec<T, S>`] stores pointer +
//! count and is not.
//!
//! ## Uninit construction handles
//!
//! [`CBoxUninit<T>`] models the allocated-but-uninitialised phase. Its `Drop`
//! runs the storage-only [`CDroppedUninit::c_drop_uninit`], never
//! [`CDropped::c_drop`] — a real destructor over fields with no valid bit
//! pattern is UB. So a construction failure before `assume_init` is plain RAII:
//! bail with `?` and the allocation is reclaimed. Both
//! [`assume_init`](CBoxUninit::assume_init) and
//! [`into_raw_uninit`](CBoxUninit::into_raw_uninit) `mem::forget` the handle so
//! the storage-only free never runs on a graduated object.

use core::fmt;
use core::marker::PhantomData;
use core::mem::MaybeUninit;
use core::ops::Deref;
use core::ptr::NonNull;

use crate::c_type::{CCell, CType};
use crate::traits::{
    CCloned, CCloner, CDropped, CDroppedUninit, CDropper, CLenCloned, CLenDropped, CValued,
};

// ---------------------------------------------------------------------------
// Abort helper — portable across std and no_std builds
// ---------------------------------------------------------------------------

/// Abort the process unconditionally. Delegates to [`std::process::abort`]
/// with `std`, and to a double-panic without it.
#[cfg(feature = "std")]
#[cold]
#[inline(never)]
fn abort_process() -> ! {
    std::process::abort()
}

#[cfg(not(feature = "std"))]
#[cold]
#[inline(never)]
fn abort_process() -> ! {
    // No guaranteed abort primitive on stable no_std. A double-panic aborts on
    // every platform: the second fires while the first is unwinding. Call sites
    // carry the context for why aborting is right there.
    struct PanicOnDrop;
    impl Drop for PanicOnDrop {
        fn drop(&mut self) {
            panic!("crustify_prim: unrecoverable failure in smart-pointer operation — aborting");
        }
    }
    let _guard = PanicOnDrop;
    panic!("crustify_prim: unrecoverable failure in smart-pointer operation — aborting");
}

// ===========================================================================
// By-value & borrowed-in-place owners
// ===========================================================================

// ---------------------------------------------------------------------------
// CVal<T> / CValGuard<'a, T> — by-value / borrowed-in-place dispose
// ---------------------------------------------------------------------------

/// By-value owning storage for a [`CValued`] type: holds `T` inline, no heap
/// allocation for the header, and runs `T::c_dispose` on drop — the
/// inline-storage analogue of [`CBox`].
///
/// `#[repr(transparent)]` over `T` (= the C struct layout), so it may itself be
/// embedded by value in a `#[repr(C)]` parent *when Rust owns the resource*.
/// When the C parent owns teardown, embed the bare `T`, which has no `Drop`.
///
/// Build the inner `T` with `T::zeroed()`, or `T::uninit()` followed by a C
/// constructor on `T::as_ptr()` (reachable through `Deref`) before reading any
/// field.
#[repr(transparent)]
pub struct CVal<T: CValued + CCell> {
    inner: T,
}

impl<T: CValued + CCell> CVal<T> {
    /// Wrap an already-constructed value, e.g.
    /// `CVal::new(GitOidarray::zeroed())`. For the C-init path, pass
    /// `cv.as_ptr()` (resolved via `Deref` to `T::as_ptr()`, a `*mut C`) to
    /// the C constructor before reading any field.
    #[inline]
    pub fn new(inner: T) -> Self {
        Self { inner }
    }
}

impl<T: CValued + CCell> fmt::Debug for CVal<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("CVal").field(&self.inner.as_ptr()).finish()
    }
}

impl<T: CValued + CCell> Deref for CVal<T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<T: CValued + CCell> Drop for CVal<T> {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: we uniquely own `inner`, a live initialised `T`; `c_dispose`
        // disposes its resource once and leaves our inline header alone.
        unsafe { T::c_dispose(NonNull::from(&mut self.inner)) }
    }
}

/// Borrow-path analogue of [`CVal`]: an in-place dispose-on-drop guard over a
/// value it does *not* own — an embedded, address-pinned sub-object whose
/// header belongs to a parent. On drop it runs `T::c_dispose` in place, never
/// freeing or moving the header, unless [`dismiss`](Self::dismiss)ed.
///
/// Separate from [`CVal`] because `CVal` owns `T` inline, so every consuming
/// operation *moves* it — unsound for an address-sensitive C type (one
/// embedding a mutex or a self-pointer). `CValGuard` holds a `NonNull<T>`, so
/// arming, dropping and dismissing move only the pointer and the pointee stays
/// pinned. The ScopeGuard pattern for an embedded sub-object whose disposal is
/// driven by an external state machine.
pub struct CValGuard<'a, T: CValued + CCell> {
    ptr: NonNull<T>,
    _borrow: PhantomData<&'a mut T>,
}

impl<'a, T: CValued + CCell> CValGuard<'a, T> {
    /// Arm an in-place dispose guard over a borrowed, embedded value: at scope
    /// exit `T::c_dispose` disposes its owned fields in place unless dismissed.
    ///
    /// # Safety
    ///
    /// - `value` must be a live, initialised `T` whose header is owned by a
    ///   parent that outlives `'a`.
    /// - The caller must not read the disposed/reset value as if still
    ///   initialised after the guard fires (the parent typically re-initialises
    ///   it before reuse).
    #[inline]
    pub unsafe fn new(value: &'a mut T) -> Self {
        Self {
            ptr: NonNull::from(value),
            _borrow: PhantomData,
        }
    }

    /// Arm the guard from a write-provenance pointer — the `&self`
    /// interior-mutability path, where the sub-object is reached through its
    /// wrapper's `as_ptr()` rather than an exclusive `&mut`. `None` if null.
    ///
    /// # Safety
    ///
    /// - `ptr` must be a live, initialised `T` whose header is owned elsewhere
    ///   and outlives `'a`. A raw pointer does not constrain `'a`, so the caller
    ///   must ensure the guard does not outlive the borrowed value.
    /// - As [`new`](Self::new): the disposed value must not afterwards be read
    ///   as if still initialised.
    #[inline]
    pub unsafe fn from_ptr(ptr: *mut T) -> Option<Self> {
        NonNull::new(ptr).map(|ptr| Self {
            ptr,
            _borrow: PhantomData,
        })
    }

    /// Cancel the disposal, leaving the borrowed value intact for its owner.
    ///
    /// Sound precisely because the guard does not own `T`: `mem::forget` moves
    /// only the pointer, so an address-sensitive pointee is not relocated.
    #[inline]
    pub fn dismiss(self) {
        core::mem::forget(self);
    }
}

impl<T: CValued + CCell> fmt::Debug for CValGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("CValGuard")
            .field(&self.ptr.as_ptr())
            .finish()
    }
}

impl<'a, T: CValued + CCell> Deref for CValGuard<'a, T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        // SAFETY: `ptr` borrows a live, initialised `T` for `'a`.
        unsafe { self.ptr.as_ref() }
    }
}

impl<'a, T: CValued + CCell> Drop for CValGuard<'a, T> {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: `ptr` is a live, borrowed, initialised `T`; `c_dispose` works
        // in place — no header free, no move. Reaching here means the guard was
        // not dismissed, since `dismiss` forgets it.
        unsafe { T::c_dispose(self.ptr) }
    }
}

// ===========================================================================
// Exclusive (unique) owners
// ===========================================================================

// ---------------------------------------------------------------------------
// CBox<T> — C-allocated objects with destructor
// ---------------------------------------------------------------------------

/// Owned smart pointer for C-allocated objects with a destructor. `Drop` calls
/// [`CDropped::c_drop`]; `Clone` is opt-in via [`CCloned`], with
/// [`try_clone`](Self::try_clone) for the fallible form.
///
/// `#[repr(transparent)]` over [`NonNull<T>`] — 8 bytes on 64-bit, ABI- and
/// layout-compatible with `*mut T`, and `Option<CBox<T>>` likewise via the
/// `NonNull` niche.
///
/// Serves a type-specific destructor (`CBox<EvpMdCtx>` over `EVP_MD_CTX_free`)
/// and a generic allocator free alike — for the latter, register `OPENSSL_free`
/// on a `#[repr(transparent)]` byte newtype and `CBox` stays layout-compatible
/// with `*mut u8`. Conditional teardown folds into `c_drop` (see [`CDropped`]).
#[repr(transparent)]
#[must_use = "dropping a CBox runs the destructor"]
pub struct CBox<T: CDropped + CCell> {
    ptr: NonNull<T>,
}

impl<T: CDropped + CCell> CBox<T> {
    /// Take ownership of a raw pointer; `None` if null.
    ///
    /// # Safety
    ///
    /// - `ptr` must be valid (or null).
    /// - The caller must transfer unique ownership; the resulting `Drop` runs
    ///   `T::c_drop`.
    #[inline]
    pub unsafe fn from_raw(ptr: *mut T::C) -> Option<Self> {
        NonNull::new(ptr.cast::<T>()).map(|ptr| Self { ptr })
    }

    /// Raw pointer for passing to C. Ownership is retained.
    #[inline]
    #[must_use]
    pub fn as_ptr(&self) -> *mut T::C {
        self.ptr.as_ptr().cast::<T::C>()
    }

    /// Consume the `CBox` without running the destructor; the caller becomes
    /// responsible for freeing the object.
    #[inline]
    #[must_use = "the returned pointer owns the object and must be freed"]
    pub fn into_raw(self) -> *mut T::C {
        let ptr = self.ptr.as_ptr().cast::<T::C>();
        core::mem::forget(self);
        ptr
    }
}

impl<T: CDropped + CCell> Drop for CBox<T> {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: `self.ptr` is a live `T` we uniquely own, released once.
        unsafe { T::c_drop(self.ptr) }
    }
}

impl<T: CDropped + CCell> Deref for CBox<T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        // SAFETY: `self.ptr` is non-null and points to a live `T`.
        unsafe { self.ptr.as_ref() }
    }
}

impl<T: CCloned + CCell> CBox<T> {
    /// Fallible clone: [`CCloned::c_clone`], or `None` if the C routine failed.
    ///
    /// The difference from [`Clone::clone`] is the failure path — this returns
    /// `None`, matching what the C routine reports (a NULL from a `*_dup`, a
    /// failing status from an `up_ref`); `Clone` aborts, because the trait is
    /// infallible. Use `try_clone` in ported code that checked that return
    /// value and propagated the error:
    ///
    /// ```ignore
    /// // C: if ((copy = EVP_PKEY_dup(src)) == NULL) { ERR_raise(...); return NULL; }
    /// let copy = pkey_box.try_clone().ok_or(Error::DupFailed)?;
    /// ```
    ///
    /// Use [`Clone`] everywhere else — it is what `#[derive(Clone)]` and
    /// `T: Clone` bounds hook into.
    #[inline]
    pub fn try_clone(&self) -> Option<Self> {
        // SAFETY: `self.ptr` is a live `T`, and the `CCloned` contract makes a
        // `Some` return a handle owing exactly one `c_drop`.
        unsafe { T::c_clone(self.ptr) }.map(|ptr| Self { ptr })
    }
}

impl<T: CCloned + CCell> Clone for CBox<T> {
    #[inline]
    fn clone(&self) -> Self {
        // Failure aborts rather than fabricating a handle: `Clone` is
        // infallible by trait contract, and a panic would be swallowed by a
        // `catch_unwind` at the FFI boundary. Use `try_clone` for the
        // recoverable path.
        match self.try_clone() {
            Some(b) => b,
            None => abort_process(),
        }
    }
}

impl<T: CDropped + CCell> fmt::Debug for CBox<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("CBox").field(&self.ptr.as_ptr()).finish()
    }
}

impl<T: CDropped + CCell> fmt::Pointer for CBox<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Pointer::fmt(&self.ptr.as_ptr(), f)
    }
}

// ---------------------------------------------------------------------------
// CBoxUninit<T> — construction-phase handle (pre-initialisation)
// ---------------------------------------------------------------------------

/// Exclusive ownership of an allocated slot that does **not yet** hold a valid
/// `T` — the type-level signal for "allocated but not initialised", one rung
/// below [`CBox<T>`], whose contract requires a fully-formed pointee.
///
/// For a Rust-driven construction sequence against a C allocator: allocate raw
/// memory, [`from_raw_uninit`](Self::from_raw_uninit), fill fields through
/// [`as_mut_ptr`](Self::as_mut_ptr), then [`assume_init`](Self::assume_init) to
/// promote — which swaps the storage-only teardown for the full one.
///
/// `Drop` runs [`CDroppedUninit::c_drop_uninit`], never `T::c_drop`: the real
/// destructor inspects fields (sub-pointers, refcount cells) that do not yet
/// hold valid bit patterns. See the [module docs](self#uninit-construction-handles).
#[repr(transparent)]
#[must_use = "a CBoxUninit should be consumed via `assume_init` (once the slot \
              has been initialised) or `into_raw_uninit`; dropping it runs the \
              storage-only `c_drop_uninit`, reclaiming the allocation but \
              discarding the partially-built object"]
pub struct CBoxUninit<T: CDroppedUninit + CCell> {
    ptr: NonNull<T>,
}

impl<T: CDroppedUninit + CCell> CBoxUninit<T> {
    /// Wrap a freshly-allocated, potentially-uninitialised slot; `None` if
    /// null. `ptr` is typed `*mut MaybeUninit<T>` so the call site shows the
    /// pointee is not yet a valid `T`.
    ///
    /// # Safety
    ///
    /// - `ptr` must be a valid allocation matching `T`'s size and alignment,
    ///   typically from a C allocator (`CRYPTO_zalloc`, `malloc`, `kmalloc`).
    /// - No other handle, Rust or C-side, may alias it.
    /// - The caller must either initialise the slot and
    ///   [`assume_init`](Self::assume_init), or free the allocation after
    ///   [`into_raw_uninit`](Self::into_raw_uninit).
    #[inline]
    pub unsafe fn from_raw_uninit(ptr: *mut MaybeUninit<T>) -> Option<Self> {
        // SAFETY: MaybeUninit<T> has the same layout as T, so the cast is
        // sound. We then re-wrap the non-null pointer.
        NonNull::new(ptr.cast::<T>()).map(|ptr| Self { ptr })
    }

    /// Raw pointer to the slot for in-place initialisation — C init functions
    /// or raw field projections. Valid until the handle is consumed.
    #[inline]
    #[must_use]
    pub fn as_mut_ptr(&mut self) -> *mut T {
        self.ptr.as_ptr()
    }

    /// Consume the handle, asserting the slot now holds a valid `T`.
    ///
    /// # Safety
    ///
    /// All of `T`'s validity invariants must hold for the slot's bytes, as
    /// must every invariant the C library expects — including any state
    /// `T::c_drop` reads (sub-allocations, refcount cells). The returned
    /// [`CBox<T>`] will run it on drop, which is UB on a partial `T`.
    #[inline]
    pub unsafe fn assume_init(self) -> CBox<T>
    where
        T: CDropped,
    {
        let ptr = self.ptr;
        // `mem::forget` suppresses the storage-only `Drop`; ownership of the now
        // fully-formed object passes to `CBox`, whose `Drop` runs `T::c_drop`.
        core::mem::forget(self);
        CBox { ptr }
    }

    /// Consume the handle and return the pointer, running no cleanup. Typed
    /// `*mut MaybeUninit<T>` to make the absent validity claim explicit; the
    /// caller must free the allocation.
    ///
    /// For a construction error path where the slot is incomplete, so
    /// `assume_init` would be unsound and the allocator's free must run
    /// directly.
    #[inline]
    #[must_use = "the returned pointer owns the allocation; it must be freed"]
    pub fn into_raw_uninit(self) -> *mut MaybeUninit<T> {
        let ptr = self.ptr.as_ptr().cast::<MaybeUninit<T>>();
        core::mem::forget(self);
        ptr
    }
}

// `T: CCell` makes `&T` over a not-yet-formed slot sound (`MaybeUninit`
// suppresses the validity invariant, `UnsafeCell` the `noalias`), so
// construction code can initialise fields through the same interior-mutable
// `&self` accessors a formed `CBox` uses. No `DerefMut`, ever. Reading a field
// not yet written is still UB unless the allocator zeroed the slot.
impl<T: CDroppedUninit + CCell> Deref for CBoxUninit<T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        // SAFETY: `T: CCell` ⇒ transparent over `UnsafeCell<MaybeUninit<_>>`,
        // valid for any bit pattern, so `&T` over the slot is sound.
        unsafe { self.ptr.as_ref() }
    }
}

impl<T: CDroppedUninit + CCell> Drop for CBoxUninit<T> {
    /// Storage-only cleanup for an unconsumed handle — no field teardown, since
    /// the slot may be partial. `assume_init` / `into_raw_uninit` forget the
    /// handle, so this never double-frees a graduated object.
    #[inline]
    fn drop(&mut self) {
        // SAFETY: per `from_raw_uninit`, `self.ptr` is a live, uniquely-owned
        // allocation, and consumers forget us rather than reaching here.
        unsafe { T::c_drop_uninit(self.ptr) }
    }
}

impl<T: CDroppedUninit + CCell> fmt::Debug for CBoxUninit<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("CBoxUninit")
            .field(&self.ptr.as_ptr())
            .finish()
    }
}

impl<T: CDroppedUninit + CCell> fmt::Pointer for CBoxUninit<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Pointer::fmt(&self.ptr.as_ptr(), f)
    }
}

// ===========================================================================
// Type-erased, string, and buffer owners
// ===========================================================================

// ---------------------------------------------------------------------------
// CVoidBox<D> — type-erased owned FFI pointer (void*) with a static deleter class
// ---------------------------------------------------------------------------

/// Owned, **type-erased** FFI pointer: a `*mut c_void` Rust owns and frees on
/// drop via the deleter class `D`. The companion to [`CBox<T>`] for a pointer
/// that stays opaque for its entire journey through C — a callback's
/// `void *payload`, a blob handed to a generic slot — and never materialises
/// back into a concrete Rust type.
///
/// Pure ownership: hold it, hand the `void *` to C ([`as_ptr`](Self::as_ptr)),
/// take it back ([`into_raw`](Self::into_raw) / [`from_raw`](Self::from_raw)),
/// free it. The bytes are opaque; the only thing known is how to free them.
///
/// `D` is **not** the pointee type — it names the *destructor class*, so two
/// erased pointers freed by the same routine share one `D`. Alias it for
/// readability: `type GitOwnedBuf = CVoidBox<GitMallocFree>;`.
///
/// `#[repr(transparent)]` over [`NonNull<c_void>`](core::ptr::NonNull) with `D`
/// in a zero-sized [`PhantomData`], so the layout is exactly a C `void *` (and
/// `Option<CVoidBox<D>>` is the null-niche `void *`) — it drops straight into a
/// `void *` field or parameter. `D` rides along in the type, so reclaiming
/// needs only the pointer.
#[repr(transparent)]
pub struct CVoidBox<D: CDropped> {
    ptr: NonNull<core::ffi::c_void>,
    _deleter: PhantomData<D>,
}

impl<D: CDropped> CVoidBox<D> {
    /// Take ownership of a raw `void *`. Returns `None` if `ptr` is null.
    ///
    /// # Safety
    ///
    /// - `ptr` must be valid (or null) and uniquely owned.
    /// - `D::c_drop` must be the correct destructor for `ptr`'s allocation.
    /// - The resulting `Drop` will run `D::c_drop(ptr)` exactly once.
    #[inline]
    pub unsafe fn from_raw(ptr: *mut core::ffi::c_void) -> Option<Self> {
        NonNull::new(ptr).map(|ptr| Self {
            ptr,
            _deleter: PhantomData,
        })
    }

    /// Borrow the erased `*mut c_void` for passing to C. Ownership is retained.
    #[inline]
    #[must_use]
    pub fn as_ptr(&self) -> *mut core::ffi::c_void {
        self.ptr.as_ptr()
    }

    /// Consume the handle without running the destructor. Reclaim the pointer
    /// with [`from_raw`](Self::from_raw) — `D` is implied by the type — or hand
    /// it to its C free routine; otherwise it leaks.
    #[inline]
    #[must_use = "the returned pointer owns the object and must be freed"]
    pub fn into_raw(self) -> *mut core::ffi::c_void {
        let ptr = self.ptr.as_ptr();
        core::mem::forget(self);
        ptr
    }
}

impl<D: CDropped> Drop for CVoidBox<D> {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: we uniquely own the allocation and `D` names its destructor
        // class. `D` is zero-sized, so the cast just retypes the address, whose
        // `c_drop` releases it exactly once.
        unsafe { D::c_drop(self.ptr.cast::<D>()) }
    }
}

impl<D: CDropped> fmt::Debug for CVoidBox<D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("CVoidBox").field(&self.ptr.as_ptr()).finish()
    }
}

impl<D: CDropped> fmt::Pointer for CVoidBox<D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Pointer::fmt(&self.ptr.as_ptr(), f)
    }
}

// ---------------------------------------------------------------------------
// CrustifyStr<D> — owned NUL-terminated C string with a cleanup strategy
// ---------------------------------------------------------------------------

/// Owned, **NUL-terminated** C string freed on drop through a pluggable deleter
/// `D` — the string analogue of [`CVoidBox`], same thin owned-pointer shape but
/// with read-only string views ([`as_c_str`](Self::as_c_str) /
/// [`as_bytes`](Self::as_bytes) / [`to_str`](Self::to_str)) over the seam. A
/// `CString` freed by a C allocator rather than Rust's.
///
/// `D: CDropped` names the destructor class, as for [`CVoidBox`]. Since
/// `strlen` always recovers the length, one `D` covers both a plain free and a
/// length-aware clearing free, so no [`CLenDropped`] variant is needed. Alias
/// per family: `type OpensslStrdup = CrustifyStr<CryptoStrdupFree>;`.
///
/// `#[repr(transparent)]` over [`NonNull<CType<c_char>>`](CType), so the layout
/// is exactly a C `char *` (and `Option<_>` is the null-niche `char *`).
///
/// Views are **read-only**, matching [`core::ffi::CStr`]: a `&mut [u8]` into
/// the contents could write an interior NUL (silently truncating) or clobber
/// the terminator (unbounded-read UB in every C consumer). Mutate by
/// rebuilding.
#[repr(transparent)]
pub struct CrustifyStr<D: CDropped> {
    ptr: NonNull<CType<core::ffi::c_char>>,
    _deleter: PhantomData<D>,
}

impl<D: CDropped> CrustifyStr<D> {
    /// Take ownership of a raw C string. Returns `None` if `ptr` is null.
    ///
    /// # Safety
    ///
    /// - `ptr` must be a valid, NUL-terminated, uniquely-owned C string.
    /// - `D::c_drop` must be the correct destructor for `ptr`'s allocation.
    /// - The resulting `Drop` runs `D::c_drop(ptr)` exactly once.
    #[inline]
    pub unsafe fn from_raw(ptr: *mut core::ffi::c_char) -> Option<Self> {
        NonNull::new(CType::cast_from(ptr).cast_mut()).map(|ptr| Self {
            ptr,
            _deleter: PhantomData,
        })
    }

    /// Borrow the raw `*const c_char` for passing to C. Ownership is retained.
    ///
    /// `*const`, not `*mut` like the other owners' `as_ptr`: the views are
    /// read-only, so this mirrors [`CStr::as_ptr`](core::ffi::CStr::as_ptr).
    /// [`into_raw`](Self::into_raw) is the `*mut` path.
    #[inline]
    #[must_use]
    pub fn as_ptr(&self) -> *const core::ffi::c_char {
        CType::cast_into(self.ptr.as_ptr()).cast_const()
    }

    /// Consume without freeing, surrendering the raw `*mut c_char`. Reclaim it
    /// with [`from_raw`](Self::from_raw) or hand it to its C free routine;
    /// otherwise the allocation leaks.
    #[inline]
    #[must_use = "the returned pointer owns the string and must be freed"]
    pub fn into_raw(self) -> *mut core::ffi::c_char {
        let ptr = CType::cast_into(self.ptr.as_ptr());
        core::mem::forget(self);
        ptr
    }

    /// Borrowed [`core::ffi::CStr`] view (computes `strlen`), bound to `&self`.
    #[inline]
    #[must_use]
    pub fn as_c_str(&self) -> &core::ffi::CStr {
        // SAFETY: the type invariant guarantees a live, NUL-terminated string
        // at `self.ptr`; the returned view is bound to `&self`.
        unsafe { core::ffi::CStr::from_ptr(self.as_ptr()) }
    }

    /// The string bytes, **excluding** the terminating NUL (bytes up to the
    /// first NUL, matching [`CStr::to_bytes`](core::ffi::CStr::to_bytes)).
    #[inline]
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        self.as_c_str().to_bytes()
    }

    /// The `strlen` of the string (byte length, excluding the NUL).
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.as_bytes().len()
    }

    /// Whether the string is empty (its first byte is the NUL terminator).
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.as_bytes().is_empty()
    }

    /// Decode as UTF-8, borrowing `&self`. `Err` if the bytes are not UTF-8.
    #[inline]
    pub fn to_str(&self) -> Result<&str, core::str::Utf8Error> {
        self.as_c_str().to_str()
    }
}

impl<D: CDropped> Drop for CrustifyStr<D> {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: as `CVoidBox::drop` — we uniquely own the allocation, and the
        // zero-sized `D` names the destructor class that releases it once.
        unsafe { D::c_drop(self.ptr.cast::<D>()) }
    }
}

impl<D: CDropped> fmt::Debug for CrustifyStr<D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("CrustifyStr").field(&self.as_ptr()).finish()
    }
}

impl<D: CDropped> fmt::Pointer for CrustifyStr<D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Pointer::fmt(&self.as_ptr(), f)
    }
}

// `Clone` only when the strategy registers a copy via `CCloned`, so a
// `CDropped`-only strategy fails to compile rather than silently making a
// shallow, double-freeing copy. `CCloned` fits directly: a `strdup` is
// `c_clone(ptr) -> ptr` with the length recovered by `strlen`, so unlike
// `CVec` no length-aware variant is needed.
impl<D: CCloned> CrustifyStr<D> {
    /// Fallible deep clone: `strdup`-style copy via the strategy's
    /// [`CCloned::c_clone`]. `None` if the C copy fails (e.g. OOM) — use where
    /// the original C checked a `*_strdup` return.
    #[inline]
    pub fn try_clone(&self) -> Option<Self> {
        // SAFETY: `self.ptr` is a live NUL-terminated string; `c_clone` dups it
        // into a fresh, independently-owned allocation releasable by `D`.
        let ptr = unsafe { D::c_clone(self.ptr.cast::<D>())? };
        Some(Self {
            ptr: ptr.cast::<CType<core::ffi::c_char>>(),
            _deleter: PhantomData,
        })
    }
}

impl<D: CCloned> Clone for CrustifyStr<D> {
    #[inline]
    fn clone(&self) -> Self {
        // Abort on the C-copy-failed (`None`) case; see `CVec`/`CBox::clone`.
        match self.try_clone() {
            Some(s) => s,
            None => abort_process(),
        }
    }
}

// ---------------------------------------------------------------------------
// CVec<T, S> — length-aware buffers with cleanup strategy
// ---------------------------------------------------------------------------

/// Owned smart pointer for a C-allocated array with a known element count.
/// `Drop` calls the strategy's [`c_drop_len`](CLenDropped::c_drop_len) with the
/// total byte length.
///
/// 16 bytes on 64-bit (`NonNull<T>` + `usize`), so **not** layout-compatible
/// with `*mut T` — for that, use a [`CBox`] over a transparent newtype. The
/// strategy `S` is a compile-time parameter, so plain free, secure zero+free
/// and zero-only cost nothing at runtime.
pub struct CVec<T, S: CLenDropped> {
    ptr: NonNull<T>,
    count: usize,
    _strategy: PhantomData<fn() -> S>,
}

impl<T, S: CLenDropped> CVec<T, S> {
    /// Wrap an existing C-allocated array of `count` elements; `None` if null.
    ///
    /// # Safety
    ///
    /// - `ptr` must hold at least `count` contiguous `T`, from the allocator
    ///   strategy `S` frees.
    /// - The caller must transfer unique ownership.
    #[inline]
    pub unsafe fn from_raw_parts(ptr: *mut T, count: usize) -> Option<Self> {
        NonNull::new(ptr).map(|ptr| Self {
            ptr,
            count,
            _strategy: PhantomData,
        })
    }

    /// Return the raw pointer.
    #[inline]
    #[must_use]
    pub fn as_ptr(&self) -> *mut T {
        self.ptr.as_ptr()
    }

    /// Number of elements.
    #[inline]
    #[must_use]
    pub fn count(&self) -> usize {
        self.count
    }

    /// Whether the buffer holds zero elements.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Total size in bytes (`count * size_of::<T>()`).
    #[inline]
    #[must_use]
    pub fn byte_len(&self) -> usize {
        self.count.wrapping_mul(core::mem::size_of::<T>())
    }

    /// View as an immutable typed slice.
    #[inline]
    #[must_use]
    pub fn as_slice(&self) -> &[T] {
        // SAFETY: `count` contiguous elements at `ptr` per the invariants; the
        // slice is bound by `&self`.
        unsafe { core::slice::from_raw_parts(self.ptr.as_ptr(), self.count) }
    }

    /// View as a mutable typed slice.
    #[inline]
    #[must_use]
    pub fn as_mut_slice(&mut self) -> &mut [T] {
        // SAFETY: as `as_slice`, with `&mut self` making it exclusive.
        unsafe { core::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.count) }
    }

    /// Consume without running cleanup, returning pointer and element count.
    #[inline]
    #[must_use = "the returned pointer owns the allocation and must be freed"]
    pub fn into_raw_parts(self) -> (*mut T, usize) {
        let result = (self.ptr.as_ptr(), self.count);
        core::mem::forget(self);
        result
    }
}

impl<T, S: CLenDropped> Drop for CVec<T, S> {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: `byte_len` bytes at `ptr`, allocated compatibly with `S`.
        unsafe { S::c_drop_len(self.ptr.as_ptr().cast::<u8>(), self.byte_len()) }
    }
}

// `Clone` only when the strategy registers a copy via `CLenCloned`, so a
// `CLenDropped`-only strategy fails to compile rather than silently making a
// shallow, double-freeing copy.
impl<T, S: CLenCloned> CVec<T, S> {
    /// Fallible deep clone: byte-copies the buffer via the strategy's
    /// [`CLenCloned::c_clone_len`]. `None` if the C copy fails (e.g. OOM) — use
    /// where the original C checked a `*_memdup` return. **Shallow**: sound
    /// only for POD `T` (see [`CLenCloned`]).
    #[inline]
    pub fn try_clone(&self) -> Option<Self> {
        // SAFETY: `self.ptr` holds `byte_len()` live bytes; `c_clone_len`
        // returns a fresh, independently-owned copy releasable by `S`.
        let ptr = unsafe { S::c_clone_len(self.ptr.as_ptr().cast::<u8>(), self.byte_len())? };
        Some(Self {
            ptr: ptr.cast::<T>(),
            count: self.count,
            _strategy: PhantomData,
        })
    }
}

impl<T, S: CLenCloned> Clone for CVec<T, S> {
    #[inline]
    fn clone(&self) -> Self {
        // Abort rather than fabricate a buffer; see `CBox::clone`.
        match self.try_clone() {
            Some(v) => v,
            None => abort_process(),
        }
    }
}

impl<T, S: CLenDropped> fmt::Debug for CVec<T, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CVec")
            .field("ptr", &self.ptr.as_ptr())
            .field("count", &self.count)
            .field("byte_len", &self.byte_len())
            .finish()
    }
}

// No auto `Send`/`Sync`: thread-safety depends on `T` and on what the allocator
// does. Add `unsafe impl`s per concrete instantiation once verified.

// ===========================================================================
// CBoxWith — the owner that carries its teardown policy as a value
// ===========================================================================
//
// The distinction from the thin `CBox` is one fact seen twice: `CDropped` is a
// *type-level* contract (`c_drop(ptr)`, an associated fn), while `CDropper` is
// a *value-level* one (`c_drop(&self, ptr)`, a method). Teardown recoverable
// from the type alone needs no storage, so `CBox` stays `#[repr(transparent)]`
// over `NonNull<T>` and reconstructs from a bare pointer. Teardown that is a
// property of a value must store that value — so the owner is fat, and
// `from_raw` needs the value passed in, because no generic code can invent one.
//
// The canonical case is `OPENSSL_sk_pop_free(stack, elem_free_fn)`, whose
// element-free function the caller picks at the wrapping site. Bolting a
// generic `State` field onto `CBox` is not an option: the compiler cannot prove
// a generic field is a 1-ZST, so `#[repr(transparent)]` would be lost for every
// user. Hence a separate `#[repr(C)]` owner storing `{ptr, dropper: D}`, where
// `D` implements `CDropper` / `CCloner` — the agent-noun analogues of the
// passive `CDropped` / `CCloned` pair.
//
// As with the thin owner, a refcounted pointee needs no separate type: register
// the down-ref as `CDropper::c_drop` and the `up_ref` as `CCloner::c_clone`.
//
// A ZST `D` collapses back to the thin layout under `repr(C)`; a stateful one
// is genuinely fat and no longer a bare pointer. Reach for `CBoxWith` when
// teardown is not recoverable from `T` alone: runtime state, or a second policy
// for one C type. Otherwise `CBox`.

// ---------------------------------------------------------------------------
// CBoxWith<T, D> — unique owner + inline teardown state
// ---------------------------------------------------------------------------

/// Unique owner whose teardown is carried by a **policy object** `D` rather
/// than recovered from `T` — the sibling of [`CBox`]. Stores `{ptr, dropper: D}`
/// and runs `D::c_drop(&self, ptr)` on drop; `Clone` is opt-in via [`CCloner`]
/// plus `D: Clone`.
///
/// `#[repr(C)]`, so a ZST `D` collapses to pointer size while a stateful one is
/// genuinely fat and no longer layout-compatible with `*mut T::C`. Reach for it
/// when teardown is not recoverable from `T` alone: runtime state, or a second
/// policy for one C type — a ZST `D` keeps the pointer-compatible layout.
/// Otherwise [`CBox`].
///
/// The seam speaks the raw C type exactly like [`CBox`]; only
/// [`from_raw`](Self::from_raw) gains the `dropper` argument — the point at
/// which the teardown policy is fixed.
#[repr(C)]
#[must_use = "dropping a CBoxWith runs the destructor strategy"]
pub struct CBoxWith<T: CCell, D: CDropper<T>> {
    ptr: NonNull<T>,
    dropper: D,
}

impl<T: CCell, D: CDropper<T>> CBoxWith<T, D> {
    /// Take ownership of a raw pointer plus its teardown state; `None` if null.
    ///
    /// # Safety
    ///
    /// - `ptr` must be valid (or null).
    /// - The caller must transfer unique ownership; the resulting `Drop` runs
    ///   `dropper.c_drop(ptr)`.
    #[inline]
    pub unsafe fn from_raw(ptr: *mut T::C, dropper: D) -> Option<Self> {
        NonNull::new(ptr.cast::<T>()).map(|ptr| Self { ptr, dropper })
    }

    /// Raw pointer for passing to C. Ownership is retained.
    #[inline]
    #[must_use]
    pub fn as_ptr(&self) -> *mut T::C {
        self.ptr.as_ptr().cast::<T::C>()
    }

    /// Borrow the inline teardown state.
    #[inline]
    pub fn dropper(&self) -> &D {
        &self.dropper
    }

    /// Consume without running the strategy, returning the raw pointer and the
    /// recovered state to re-wrap or free manually. The caller becomes
    /// responsible for freeing the object.
    #[inline]
    #[must_use = "the returned pointer owns the object and must be freed"]
    pub fn into_raw(self) -> (*mut T::C, D) {
        let this = core::mem::ManuallyDrop::new(self);
        let ptr = this.ptr.as_ptr().cast::<T::C>();
        // SAFETY: `ManuallyDrop` means `this` is never dropped, so a single
        // `read` of `dropper` neither double-drops nor leaves an alias.
        let dropper = unsafe { core::ptr::read(&this.dropper) };
        (ptr, dropper)
    }
}

impl<T: CCell, D: CDropper<T>> Drop for CBoxWith<T, D> {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: we uniquely own `ptr`; the strategy releases it once, using
        // `dropper` as teardown state.
        unsafe { self.dropper.c_drop(self.ptr) }
    }
}

impl<T: CCell, D: CDropper<T>> Deref for CBoxWith<T, D> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        // SAFETY: `self.ptr` is non-null and points to a live `T`.
        unsafe { self.ptr.as_ref() }
    }
}

impl<T: CCell, D: CCloner<T> + Clone> CBoxWith<T, D> {
    /// Fallible clone: [`CCloner::c_clone`] plus a clone of the state, or
    /// `None` if the C routine failed. See [`CBox::try_clone`].
    #[inline]
    pub fn try_clone(&self) -> Option<Self> {
        // SAFETY: `ptr` is live, and per the `CCloner` contract a `Some` return
        // owes one `c_drop` releasable by a clone of `dropper`.
        let ptr = unsafe { self.dropper.c_clone(self.ptr) }?;
        Some(Self {
            ptr,
            dropper: self.dropper.clone(),
        })
    }
}

impl<T: CCell, D: CCloner<T> + Clone> Clone for CBoxWith<T, D> {
    #[inline]
    fn clone(&self) -> Self {
        // Abort rather than fabricate a handle; see `CBox::clone`.
        match self.try_clone() {
            Some(b) => b,
            None => abort_process(),
        }
    }
}

impl<T: CCell, D: CDropper<T>> fmt::Debug for CBoxWith<T, D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("CBoxWith").field(&self.ptr.as_ptr()).finish()
    }
}

impl<T: CCell, D: CDropper<T>> fmt::Pointer for CBoxWith<T, D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Pointer::fmt(&self.ptr.as_ptr(), f)
    }
}
