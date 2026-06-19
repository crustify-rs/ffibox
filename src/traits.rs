//! Lifecycle and ownership trait **contracts** — the vocabulary that says what
//! teardown / clone means for a wrapped C type. The owning smart pointers in
//! [`owned_refs`](crate::owned_refs) consume these as bounds; the `impl_*!`
//! macros in [`macros`](crate::macros) implement them.
//!
//! The two base traits form a drop/clone pair, the clone trait a sub-trait of
//! the drop trait (a cloned handle owes the same teardown):
//!
//! | Trait        | Registers                               | Enables            |
//! |--------------|-----------------------------------------|--------------------|
//! | [`CDropped`] | `c_drop` — a `*_free` **or** a down-ref  | `CBox`, `CVoidBox` |
//! | [`CCloned`]  | `c_clone` — a `*_dup` **or** an `up_ref` | `Clone for CBox`   |
//!
//! There is no separate "shared" column: exclusive and refcounted ownership
//! differ only in *which C routine* you register, not in the handle type. See
//! [`CCloned`] for the two mechanisms it spans.
//!
//! Alongside: [`CValued`] (by-value dispose), [`CDroppedUninit`] (pre-init
//! storage free), [`CLenDropped`] / [`CLenCloned`] (length-aware buffer
//! strategies), and the `*With` strategy traits [`CDropper`] / [`CCloner`]
//! (for the fat owners that carry runtime teardown state).

use core::ptr::NonNull;

// Imported so the trait docs' intra-doc links (`[CCell]`, `[CBox]`, `[CVal]`,
// `[CBoxWith]`, …) resolve; none of them is named in a signature here.
#[allow(unused_imports)]
use crate::c_type::CCell;
#[allow(unused_imports)]
use crate::owned_refs::{CBox, CBoxUninit, CBoxWith, CVal, CVec};

// ===========================================================================
// Base lifecycle grid (drop/clone x exclusive/shared) + refcount unifier,
// uninit-phase free, and by-value dispose
// ===========================================================================

/// Destructor for a C-allocated type; the bound that [`CBox<T>`] `Drop`s on.
/// The clone half is the sub-trait [`CCloned`].
///
/// `c_drop` settles one unit of ownership debt: a plain `*_free`
/// (`EVP_MD_CTX_free`), a generic allocator free (`OPENSSL_free` on a byte
/// newtype), or the refcount **down-ref** of a shared type. The wrapper does
/// not distinguish them.
///
/// # Safety
///
/// - `c_drop` must free the object and every sub-resource it owns.
/// - Its argument must be valid — from a constructor, or transferred in via
///   `into_raw`.
///
/// # Example
///
/// ```ignore
/// unsafe impl CDropped for EvpMdCtx {
///     unsafe fn c_drop(obj: NonNull<Self>) {
///         // SAFETY: caller upholds the trait contract.
///         unsafe { EVP_MD_CTX_free(obj.as_ptr().cast()) }
///     }
/// }
/// ```
///
/// # Conditional teardown
///
/// A destructor that must be suppressed on some paths folds the gate **into
/// `c_drop`** — read the object's own state and return early. For scope-driven
/// dismissal that is not a property of the object, defuse via
/// [`CBox::into_raw`].
pub unsafe trait CDropped {
    /// Free the object. Called unconditionally by `CBox::drop`.
    ///
    /// # Safety
    ///
    /// `obj` must point to a live, uniquely-owned instance of `Self`.
    unsafe fn c_drop(obj: NonNull<Self>);
}

/// Storage-only release for the **not-yet-formed** phase: the raw deallocator
/// paired with the byte-level allocator (`git__free` for `git__malloc`), which
/// frees only that allocation and never touches fields — they are not
/// initialised yet. [`CBoxUninit`] runs it on `Drop`, so a construction failure
/// before [`assume_init`](CBoxUninit::assume_init) unwinds via RAII instead of
/// leaking.
///
/// Distinct from [`CDropped::c_drop`], the *formed*-phase teardown that also
/// disposes fields (or down-refs). A heap type implements both.
///
/// # Safety
///
/// [`c_drop_uninit`](Self::c_drop_uninit) must release exactly the raw
/// allocation and run **no** field teardown, so that calling it on an
/// allocated-but-uninitialised slot is sound.
pub unsafe trait CDroppedUninit {
    /// Free the raw storage at `obj`, running **no** field cleanup.
    ///
    /// # Safety
    ///
    /// `obj` must be a live allocation from this type's byte-level allocator,
    /// not yet freed or consumed, with no other live alias.
    unsafe fn c_drop_uninit(obj: NonNull<Self>);
}

/// Teardown for a C type Rust owns **by value**: disposes owned resources
/// without freeing the header, which is Rust's inline storage and is released
/// by [`CVal`].
///
/// Contrast [`CDropped`], whose header is heap-allocated and freed via
/// [`CBox`]. A type may implement both — the wrapper you pick selects which
/// teardown runs — since a C library commonly exposes both a `*_free`
/// (storage and fields) and a `*_dispose` / `*_cleanup` (fields only).
/// Register each under the matching trait; never the same function under both.
///
/// # Safety
///
/// [`c_dispose`](Self::c_dispose) must release the value's owned resources
/// exactly once and must **not** free the header, which Rust owns and will
/// reclaim itself.
pub unsafe trait CValued {
    /// Dispose the owned resource. **Does not free the header** (Rust owns it
    /// by value). Called unconditionally by [`CVal::drop`], exactly once.
    ///
    /// # Safety
    ///
    /// `this` must point to a live, uniquely-owned, **initialised** instance
    /// of `Self`.
    unsafe fn c_dispose(this: NonNull<Self>);
}

/// Handle duplication; the bound that gives [`CBox<T>`] its [`Clone`] and
/// [`CBox::try_clone`].
///
/// The contract is stated in terms of *debt*, not allocation:
/// [`c_clone`](Self::c_clone) returns a pointer owing exactly one
/// [`CDropped::c_drop`], independent of the original. That covers **both** C
/// duplication mechanisms with one trait:
///
/// | C pattern                              | `c_clone` does                     | Returns          |
/// |----------------------------------------|------------------------------------|------------------|
/// | `*_dup` deep-copies (`EVP_PKEY_dup`)   | allocate a fresh object            | the **new** ptr  |
/// | `*_up_ref` bumps a counter in place    | increment the refcount             | the **same** ptr |
///
/// Which applies is a property of the C API, not the Rust handle: both yield a
/// second `CBox<T>` that must be dropped, and `c_drop` settles the debt either
/// way. [`impl_cloned!`](crate::impl_cloned) takes the mechanism as a named
/// argument (`dup = …` / `up_ref = …`) because the C signatures differ — a
/// dup's return value *is* the new handle, an `up_ref`'s is a status and the
/// handle to keep is the original.
///
/// Sub-trait of [`CDropped`]: every clone owes the same teardown.
///
/// **A type exposing both: the `up_ref` wins.** Register the bump as `c_clone`
/// and leave the deep copy as an inherent method. On a refcounted type `Clone`
/// means "another handle to the same object" — what the C API and callers
/// expect; a silently deep-copying `Clone` would break identity comparisons and
/// double the allocation cost.
///
/// # Safety
///
/// - A `Some` return must owe **exactly one** `c_drop` beyond the one `obj`
///   already owes: a fresh fully-initialised allocation for a deep copy, an
///   actually-incremented count for a bump.
/// - `None` must mean the C routine failed (a `NULL` dup, a zero `up_ref`
///   status) — never a dangling, half-initialised, or already-freed pointer.
/// - `c_clone` must not invalidate `obj`.
/// - A deep copy must be independent of `obj`: no shared mutable state, no
///   aliased sub-allocations beyond what the C type itself treats as shared.
///
/// # Examples
///
/// ```ignore
/// // Deep copy — return the new pointer.
/// unsafe impl CCloned for EvpPkey {
///     unsafe fn c_clone(obj: NonNull<Self>) -> Option<NonNull<Self>> {
///         // SAFETY: caller upholds the trait contract.
///         NonNull::new(unsafe { EVP_PKEY_dup(obj.as_ptr().cast()) }.cast())
///     }
/// }
///
/// // Refcount bump — return the *same* pointer, `None` on overflow.
/// unsafe impl CCloned for SslSession {
///     unsafe fn c_clone(obj: NonNull<Self>) -> Option<NonNull<Self>> {
///         // SAFETY: caller upholds the trait contract.
///         (unsafe { SSL_SESSION_up_ref(obj.as_ptr().cast()) } != 0).then_some(obj)
///     }
/// }
/// ```
pub unsafe trait CCloned: CDropped {
    /// Duplicate the handle to the C object at `obj` — by deep copy or by
    /// refcount increment — returning a pointer that owes one independent
    /// [`CDropped::c_drop`], or `None` on failure.
    ///
    /// # Safety
    ///
    /// `obj` must point to a live, valid instance of `Self`. The original
    /// handle remains valid; this call must not invalidate it.
    unsafe fn c_clone(obj: NonNull<Self>) -> Option<NonNull<Self>>;
}

// ===========================================================================
// Length-aware buffer strategies (implemented on a ZST selector, not the
// element type) — drive CVec's cleanup / clone
// ===========================================================================

/// Byte-buffer cleanup strategy; the bound [`CVec<T, S>`] drops on. Implemented
/// on a **strategy selector** type (typically a ZST), not on the element type,
/// so one element type pairs with several policies — plain free, secure
/// zero-then-free, zero-only — at zero runtime cost. The crate ships none.
///
/// # Safety
///
/// - `c_drop_len` must handle the `byte_len`-byte buffer at `ptr` under
///   whatever allocator and cleanup policy the strategy represents.
/// - `ptr` must be valid and `byte_len` must equal the original allocation's
///   byte size.
///
/// # Example
///
/// ```ignore
/// pub struct SecureFree;
/// unsafe impl CLenDropped for SecureFree {
///     unsafe fn c_drop_len(ptr: *mut u8, byte_len: usize) {
///         unsafe {
///             explicit_bzero(ptr.cast(), byte_len);
///             libc::free(ptr.cast());
///         }
///     }
/// }
/// pub type SecretKey = CVec<u8, SecureFree>;
/// ```
pub unsafe trait CLenDropped {
    /// Free the `byte_len`-byte buffer at `ptr`.
    ///
    /// # Safety
    ///
    /// `ptr` must point to a valid allocation of at least `byte_len`
    /// bytes, allocated by the allocator this strategy targets.
    unsafe fn c_drop_len(ptr: *mut u8, byte_len: usize);
}

/// Deep-copy strategy for a length-aware buffer (a `memdup`): the length-aware
/// analogue of [`CCloned`], needed because a buffer copy carries a byte length
/// that a pointer-only `c_clone` cannot. Gives [`CVec<T, S>`] its [`Clone`],
/// and only on opt-in — a `CLenDropped`-only strategy is deliberately not
/// cloneable.
///
/// **Shallow**: copies bytes, not elements, so it is sound only for POD `T`
/// that owns nothing. A buffer of owning elements needs a per-element deep
/// clone, which this contract does not provide.
///
/// # Safety
///
/// `c_clone_len` must return a fresh, uniquely-owned allocation of `byte_len`
/// bytes byte-copied from `ptr` and releasable by this strategy's
/// [`CLenDropped`] impl — or `None` on allocation failure. It must not
/// invalidate `ptr`.
pub unsafe trait CLenCloned: CLenDropped {
    /// Byte-copy the `byte_len`-byte buffer at `ptr` into a fresh allocation,
    /// or `None` on failure.
    ///
    /// # Safety
    ///
    /// `ptr` must point to a live allocation of at least `byte_len` bytes
    /// compatible with this strategy's allocator.
    unsafe fn c_clone_len(ptr: *mut u8, byte_len: usize) -> Option<NonNull<u8>>;
}

// ===========================================================================
// Stateful (`*With`) teardown strategies — agent-noun analogues of the base
// pair, implemented on the state object `D`, driving CBoxWith
// ===========================================================================

/// Exclusive drop **strategy** carrying runtime state: the fat-owner analogue
/// of [`CDropped`]. Implemented by a state object `D` (a `fn`, a length, any
/// struct) stored inline on [`CBoxWith<T, D>`]; `c_drop` receives that state
/// (`&self`) alongside the pointer, so teardown can use runtime data a
/// zero-state [`CDropped`] cannot carry (e.g. `OPENSSL_sk_pop_free(ptr, fn)`).
///
/// Prefer static monomorphization: when the policy is known at compile time,
/// register a plain [`CDropped`] and use [`CBox`]. This trait earns its keep
/// only when the state is not known until runtime.
///
/// # Safety
///
/// - `c_drop` must release `ptr` and everything it owns, exactly once, using
///   only `self` as extra state.
/// - `ptr` must be valid (from a constructor or [`CBoxWith::into_raw`]).
pub unsafe trait CDropper<T> {
    /// Free the object at `ptr`, using `self` as teardown state.
    ///
    /// # Safety
    ///
    /// `ptr` must point to a live, uniquely-owned instance of `T`.
    unsafe fn c_drop(&self, ptr: NonNull<T>);
}

/// Handle duplication carrying runtime **state** — the fat-owner analogue of
/// [`CCloned`] and a sub-trait of [`CDropper`] (a clone owes the same
/// teardown). Gives [`CBoxWith<T, D>`] its `Clone` when additionally
/// `D: Clone`.
///
/// Spans the same two mechanisms as [`CCloned`]: a deep copy returning a new
/// pointer, or an `up_ref` returning the same one. A refcounted pointee needs
/// no separate strategy type — register the down-ref as
/// [`CDropper::c_drop`] and the bump here.
///
/// As with [`CDropper`], prefer static monomorphization: a compile-time policy
/// belongs in a plain [`CCloned`] on a [`CBox`].
///
/// # Safety
///
/// A `Some` return must owe **exactly one** [`CDropper::c_drop`] beyond the one
/// `ptr` already owes — a fresh, uniquely-owned allocation for a deep copy, an
/// actually-incremented count for a bump — and must be releasable by this same
/// strategy. `None` must mean the C routine failed. `c_clone` must not
/// invalidate `ptr`.
pub unsafe trait CCloner<T>: CDropper<T> {
    /// Duplicate the handle at `ptr`, using `self` as state; `None` on
    /// failure.
    ///
    /// # Safety
    ///
    /// `ptr` must point to a live instance of `T`.
    unsafe fn c_clone(&self, ptr: NonNull<T>) -> Option<NonNull<T>>;
}
