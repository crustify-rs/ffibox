//! Non-owning pointer wrappers — raw C pointers represented **without** taking
//! ownership. They carry no teardown and no lifecycle contract (contrast the
//! owning pointers in [`owned_refs`](crate::owned_refs)); they answer *how a
//! pointer is used*, not *who frees it*.
//!
//! - [`COut<'a, T>`] — the write-end of a C `*mut T` out-parameter (a
//!   `&'a mut MaybeUninit<T>` the callee writes once).

use core::marker::PhantomData;
use core::mem::MaybeUninit;
use core::ptr::NonNull;

use crate::c_type::CCell;

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
/// use ffibox::{COut, c_out};
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

impl<'a, T: CCell> CSlice<'a, T> {
    /// Borrow `len` contiguous wrapped objects starting at `ptr`.
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

    /// Shared handle to element `i`; `None` if out of range.
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
        Some(unsafe { T::ref_from_raw(NonNull::new_unchecked(self.ptr.as_ptr().add(i))) })
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
            unsafe { T::ref_from_raw(NonNull::new_unchecked(ptr.as_ptr().add(i))) }
        })
    }

    /// Raw pointer to the first element, for passing the run to C.
    #[inline]
    #[must_use]
    pub fn as_ptr(&self) -> *mut T::C {
        self.ptr.as_ptr().cast::<T::C>()
    }
}
