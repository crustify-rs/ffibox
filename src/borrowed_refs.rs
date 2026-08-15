//! Non-owning pointer wrappers — raw C pointers represented **without** taking
//! ownership. They carry no teardown and no lifecycle contract (contrast the
//! owning pointers in [`owned_refs`](crate::owned_refs)); they answer *how a
//! pointer is used*, not *who frees it*.
//!
//! - [`COut<'a, T>`] — the write-end of a C `*mut T` out-parameter (a
//!   `&'a mut MaybeUninit<T>` the callee writes once).

use core::mem::MaybeUninit;

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
