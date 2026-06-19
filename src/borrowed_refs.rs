//! Non-owning pointer wrappers — raw C pointers represented **without** taking
//! ownership. They carry no teardown and no lifecycle contract (contrast the
//! owning pointers in [`owned_refs`](crate::owned_refs)); they answer *how a
//! pointer is used*, not *who frees it*.
//!
//! - [`COut<'a, T>`] — the write-end of a C `*mut T` out-parameter (a
//!   `&'a mut MaybeUninit<T>` the callee writes once).
//! - [`SelfPtr<'this, T>`] — a typed, `'this`-tagged **shared** borrow for
//!   self-referential / sibling / parent pointers.

use core::marker::PhantomData;
use core::mem::MaybeUninit;
use core::ptr::NonNull;

// ===========================================================================
// COut<'a, T> — C scalar out-parameter slot
// ===========================================================================

/// Typed handle to a C scalar out-parameter slot — a named alias for
/// [`&'a mut MaybeUninit<T>`](MaybeUninit), plus the [`from_ptr`] helper hiding
/// the `*mut T` → `*mut MaybeUninit<T>` cast at the boundary.
///
/// C APIs return values through `*mut T` arguments (`*mut size_t`,
/// `*mut c_int`); `&'a mut MaybeUninit<T>` is Rust's type for exclusive write
/// access to a possibly-uninitialised `T`. The alias keeps the "C out-param"
/// vocabulary greppable while call sites use [`MaybeUninit::write`] directly.
///
/// An alias rather than a newtype because a newtype would add no guarantee:
/// `&'a mut MaybeUninit<T>` already carries the lifetime tag, invariance in
/// `T`, exclusive-borrow semantics, and an `Option` niche layout-compatible
/// with `*mut T`.
///
/// # Typical use
///
/// ```ignore
/// use crustify_prim::{COut, c_out};
///
/// pub unsafe extern "C" fn my_get_len(out_len: *mut usize) -> i32 {
///     if let Some(out) = unsafe { c_out::from_ptr(out_len) } {
///         out.write(computed_len);
///     }
///     1
/// }
/// ```
pub type COut<'a, T> = &'a mut MaybeUninit<T>;

/// Wrap a raw `*mut T` as a [`COut<'a, T>`]. Returns `None` if `ptr` is null.
///
/// # Safety
///
/// The caller asserts the pointer is:
///
/// - valid for writes of `T` for at least the lifetime `'a`,
/// - properly aligned for `T`,
/// - not aliased by any other reference for `'a`,
/// - referring to a slot whose current content is *not* a valid `T` that needs
///   dropping (uninitialised, or a non-`Drop` primitive — the typical
///   C-out-param case).
#[inline]
pub unsafe fn from_ptr<'a, T>(ptr: *mut T) -> Option<COut<'a, T>> {
    // SAFETY: the caller upholds the invariants above, and any `*mut T` slot
    // satisfying them is a valid `*mut MaybeUninit<T>` — same layout, weaker
    // validity requirement.
    unsafe { (ptr as *mut MaybeUninit<T>).as_mut() }
}

// ===========================================================================
// SelfPtr<'this, T> — self-referential / sibling / parent shared borrow
// ===========================================================================

/// A typed pointer into `self`, a parent, or a sibling, tagged with the `'this`
/// lifetime of the borrow it came from.
///
/// C structs hold pointers into themselves, a parent, or a sibling in an
/// intrusive container, and Rust's borrow checker cannot express "this field
/// borrows from its container" — so the field stays raw. `SelfPtr` concentrates
/// that raw pointer into one wrapper: construction is the single audit point,
/// every read goes through [`get`](Self::get), and `'this` stops the wrapper
/// outliving what it points into.
///
/// Models a **shared** borrow only: `&T` via [`get`](Self::get), `*const T` via
/// [`as_ptr`](Self::as_ptr), never `&mut T`. A mutable self-reference would
/// need proof that nothing else aliases the field, and is out of scope.
///
/// No `PhantomPinned` — the wrapper is a plain [`Copy`] pointer and moves
/// freely. Keeping the *pointee* at a stable address is the caller's concern:
/// guaranteed by the borrow in [`new`](Self::new), asserted in
/// [`from_raw`](Self::from_raw).
///
/// `#[repr(transparent)]` over [`NonNull<T>`], so `Option<SelfPtr<'this, T>>`
/// is a niche `*const T` and can replace a raw pointer field in a `#[repr(C)]`
/// struct.
#[repr(transparent)]
pub struct SelfPtr<'this, T> {
    ptr: NonNull<T>,
    _p: PhantomData<&'this T>,
}

impl<'this, T> SelfPtr<'this, T> {
    /// Derive a `SelfPtr` from a real borrow — the safe path, where the
    /// `&'this T` argument is the borrow checker's own proof that the pointee
    /// outlives `'this`.
    #[inline]
    pub fn new(target: &'this T) -> Self {
        Self {
            ptr: NonNull::from(target),
            _p: PhantomData,
        }
    }

    /// Wrap a raw pointer; `None` if null. The escape hatch for pointers C
    /// hands in, where no Rust borrow exists to derive `'this` from — prefer
    /// [`new`](Self::new) when one does.
    ///
    /// # Safety
    ///
    /// - `ptr` must point to a valid, initialised `T`.
    /// - The pointee must remain alive, and not be mutated through any other
    ///   handle, for the whole of `'this`. The caller chooses `'this`; it must
    ///   not exceed the pointee's real validity window.
    #[inline]
    pub unsafe fn from_raw(ptr: *mut T) -> Option<Self> {
        NonNull::new(ptr).map(|ptr| Self {
            ptr,
            _p: PhantomData,
        })
    }

    /// Borrow the pointee. The reference is bound to `'this`, not `&self`: the
    /// pointee is valid at least that long by construction, and the wrapper
    /// owns nothing, so dropping it invalidates nothing.
    #[inline]
    pub fn get(&self) -> &'this T {
        // SAFETY: `new` derived `ptr` from a `&'this T`, and `from_raw`'s
        // caller asserted the same for `'this`. Only shared references are ever
        // handed out, so no `&mut` aliasing arises here.
        unsafe { self.ptr.as_ref() }
    }

    /// The pointer as `*const T`, for handing to C. `*const`, not `*mut`,
    /// because this models a shared borrow: writing through it is outside the
    /// type's contract.
    #[inline]
    pub fn as_ptr(&self) -> *const T {
        self.ptr.as_ptr()
    }
}

// A plain pointer plus a lifetime tag, so `Copy` like `&T`. Hand-written rather
// than derived, to avoid a spurious `T: Copy` bound.
impl<T> Clone for SelfPtr<'_, T> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for SelfPtr<'_, T> {}

// SAFETY: grants exactly `&T`'s access — shared reads via `get` plus a
// `*const T` (itself `!Send`/`!Sync`) — so it inherits `&T`'s auto-trait rule:
// `Send`/`Sync` iff `T: Sync`.
unsafe impl<T: Sync> Send for SelfPtr<'_, T> {}
// SAFETY: see the `Send` impl above.
unsafe impl<T: Sync> Sync for SelfPtr<'_, T> {}

#[cfg(feature = "std")]
impl<T> core::fmt::Debug for SelfPtr<'_, T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_tuple("SelfPtr").field(&self.ptr).finish()
    }
}
