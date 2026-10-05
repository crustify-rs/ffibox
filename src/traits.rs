//! The lifecycle **contracts** the owners in [`refs`](crate::refs) run, plus
//! [`CElem`] (slice-safe buffer elements) and [`CCell`] (the link from a
//! layout type to its handles).
//!
//! The lifecycle traits are implemented on a **policy** — typically a ZST
//! bound to a C routine with an `impl_*!` macro — never on the pointee. That
//! lets one C type carry several destructors as several owner types, and
//! pairs each clone routine with the teardown that settles it:
//!
//! | Trait | Registers | Drives |
//! |-------|-----------|--------|
//! | [`CDrop<T>`] | `c_drop` — a `*_free`, or a refcount down-ref | [`CBox`], [`CStrBox`], [`CArc`] |
//! | [`CDupClone<T>`] | `c_dup` — a deep copy (`*_dup`, `strdup`) | their `Clone` |
//! | [`CRefClone<T>`] | `c_up_ref` — a refcount increment; `c_is_sole_owner` | [`CArc`], [`CGuardedArc`] |
//! | [`CLenDrop`] | `c_drop_len(ptr, byte_len)` | [`CVec`] |
//! | [`CLenClone`] | `c_clone_len` — a buffer memdup | its `Clone` |
//! | [`CDispose<T>`] | `c_dispose` — `*_uninit` / `*_clear` on a value | [`CVal`] |
//!
//! Every method takes `&self`, so a policy may carry runtime state (the
//! element-free function of `OPENSSL_sk_pop_free`). A ZST policy costs nothing.
//!
//! [`CGuarded`] is the exception: the object's own lock, implemented on the
//! layout type, which [`CGuardedArc`] and [`CGuardedRef`] take around every
//! access.

use core::ptr::NonNull;

// Imported so the trait docs' intra-doc links resolve; none of them is named in
// a signature here.
#[allow(unused_imports)]
use crate::refs::{CBox, CStrBox, CVal, CVec};
#[allow(unused_imports)]
use crate::shared::{CArc, CGuardedArc, CGuardedRef};

// ===========================================================================
// Pointer teardown / duplication policies
// ===========================================================================

/// Teardown policy for an owned `*mut T`; the bound [`CBox<T, D>`] and
/// [`CStrBox<D>`] drop on.
///
/// `c_drop` releases the owner's claim on the object: a plain `*_free`
/// (`EVP_MD_CTX_free`), a generic allocator free (`OPENSSL_free`), or the
/// refcount **down-ref** of one counted reference ([`CArc`], or a [`CBox`]
/// holding the only one).
///
/// Implemented on the policy `Self`, not on `T`, so one `T` may have several,
/// and the owner's type says which runs.
/// [`impl_cdrop!`](crate::impl_cdrop) binds a C routine; write the impl by
/// hand when teardown needs runtime state. A destructor that must be
/// suppressed on some paths folds the gate into `c_drop`.
///
/// # Safety
///
/// `c_drop` must release the object and every sub-resource it owns, exactly
/// once, using only `self` as extra state.
///
/// # Example
///
/// ```ignore
/// pub struct StackPopFree(unsafe extern "C" fn(*mut c_void));
/// unsafe impl CDrop<Stack> for StackPopFree {
///     unsafe fn c_drop(&self, ptr: NonNull<Stack>) {
///         // SAFETY: caller upholds the trait contract.
///         unsafe { OPENSSL_sk_pop_free(ptr.as_ptr().cast(), Some(self.0)) }
///     }
/// }
/// ```
pub unsafe trait CDrop<T> {
    /// Release the object at `ptr`. Called unconditionally by the owner's
    /// `Drop`.
    ///
    /// # Safety
    ///
    /// `ptr` must address a live `T` whose claim the caller owns.
    unsafe fn c_drop(&self, ptr: NonNull<T>);
}

/// Deep copy; the bound that gives [`CBox`] and [`CStrBox`] their [`Clone`]
/// and `try_clone`.
///
/// A sub-trait of [`CDrop`] on the same policy, so a `*_dup` is always settled
/// by its `*_free`. A deep copy is what keeps a cloned [`CBox`] the **sole**
/// owner of its object; a refcount bump is [`CRefClone`], which `CBox` never
/// uses.
///
/// # Safety
///
/// - A `Some` return must be a fresh, fully-initialised object, independent of
///   `ptr` (no shared mutable state, no aliased sub-allocations beyond what the
///   C type itself treats as immutable), that this policy's `c_drop` releases.
/// - `None` must mean the C routine failed.
/// - `c_dup` must not invalidate `ptr`.
///
/// # Example
///
/// ```ignore
/// unsafe impl CDupClone<EvpPkey> for EvpPkeyFree {
///     unsafe fn c_dup(&self, ptr: NonNull<EvpPkey>) -> Option<NonNull<EvpPkey>> {
///         // SAFETY: caller upholds the trait contract.
///         NonNull::new(unsafe { EVP_PKEY_dup(ptr.as_ptr().cast()) }.cast())
///     }
/// }
/// ```
pub unsafe trait CDupClone<T>: CDrop<T> {
    /// Deep-copy the object at `ptr`, or `None` on failure.
    ///
    /// # Safety
    ///
    /// `ptr` must address a live `T`.
    unsafe fn c_dup(&self, ptr: NonNull<T>) -> Option<NonNull<T>>;
}

/// Refcount increment, paired with the down-ref this policy's [`CDrop`]
/// performs; the bound [`CArc`] and [`CGuardedArc`] clone on.
/// [`CBox`] never clones through this trait.
///
/// [`c_is_sole_owner`](Self::c_is_sole_owner) lets an arc hand out exclusive
/// access without a lock or a copy (`get_mut`, `make_mut`) when its reference
/// is provably the only one. The default answers `false`, which is always
/// sound: `get_mut` then returns `None` and `make_mut` always copies.
///
/// # Safety
///
/// - A `true` from `c_up_ref` must mean the count was actually incremented,
///   so the object owes one more `c_drop` of this policy (or a clone of it).
/// - A `false` from `c_up_ref` must mean the C routine failed and nothing
///   changed.
/// - Neither method may invalidate `ptr`.
/// - A `true` from `c_is_sole_owner` must mean no other reference can reach
///   the object, now or later: the count is read with at least Acquire
///   ordering (so a down-ref on another thread happens-before it), and every
///   reference is counted — weak ones, and any pointer C itself keeps and
///   could `up_ref` later.
pub unsafe trait CRefClone<T>: CDrop<T> {
    /// Increment the reference count of the object at `ptr`.
    ///
    /// # Safety
    ///
    /// `ptr` must address a live `T` whose claim the caller holds.
    unsafe fn c_up_ref(&self, ptr: NonNull<T>) -> bool;

    /// Whether the caller's reference is the only one. `false` when unsure.
    ///
    /// # Safety
    ///
    /// As [`c_up_ref`](Self::c_up_ref).
    #[inline]
    unsafe fn c_is_sole_owner(&self, ptr: NonNull<T>) -> bool {
        let _ = ptr;
        false
    }
}

/// The C object's own reader/writer lock, registered on the layout type; the
/// bound the guards of [`CGuardedArc`] and [`CGuardedRef`] lock through.
///
/// The functions take the object's pointer rather than `&self`, because a
/// `&Foo` would be a reference covering the C object. An object with only a
/// mutex implements the exclusive pair; the read pair defaults to it.
/// [`impl_cguarded!`](crate::impl_cguarded) binds C routines.
///
/// # Safety
///
/// - Every C routine that touches the object's mutable state takes this lock,
///   so holding it excludes C as well as Rust.
/// - `c_lock` returns `true` only once no other guard — read or write, on any
///   thread *including this one* — is held, and blocks until then (or returns
///   `false`, as for `pthread_rwlock_*`'s `EDEADLK`); a lock that is reentrant
///   for the exclusive side would hand out two `Mut` handles.
/// - `c_read_lock` returns `true` only once no write guard is held, on any
///   thread including this one.
/// - `false` means the C lock call failed and nothing is held.
/// - Each unlock is called once, on the thread that took the matching lock.
pub unsafe trait CGuarded: CCell {
    /// Take the exclusive lock. `false` if the C call failed.
    ///
    /// # Safety
    ///
    /// `ptr` must address a live `Self`.
    unsafe fn c_lock(ptr: NonNull<Self>) -> bool;

    /// Release the exclusive lock.
    ///
    /// # Safety
    ///
    /// The calling thread must hold the exclusive lock on `ptr`.
    unsafe fn c_unlock(ptr: NonNull<Self>);

    /// Take a shared lock. Defaults to the exclusive one.
    ///
    /// # Safety
    ///
    /// As [`c_lock`](Self::c_lock).
    #[inline]
    unsafe fn c_read_lock(ptr: NonNull<Self>) -> bool {
        // SAFETY: the caller upholds `c_lock`'s contract.
        unsafe { Self::c_lock(ptr) }
    }

    /// Release a shared lock. Defaults to the exclusive one.
    ///
    /// # Safety
    ///
    /// The calling thread must hold a shared lock on `ptr` taken with
    /// [`c_read_lock`](Self::c_read_lock).
    #[inline]
    unsafe fn c_read_unlock(ptr: NonNull<Self>) {
        // SAFETY: the caller upholds the contract.
        unsafe { Self::c_unlock(ptr) }
    }
}

/// In-place disposal of a value Rust holds inline; the bound [`CVal<T, D>`]
/// drops on. Releases what the value's fields own and leaves the value's
/// storage, which is Rust's, alone: `*_uninit`, `*_clear`, `*_dispose`.
///
/// [`impl_cdispose!`](crate::impl_cdispose) binds a C routine.
///
/// # Safety
///
/// `c_dispose` must release the value's owned resources exactly once, must not
/// free the value itself, and must accept every value safe code can bring a
/// `T` to: what a safe constructor produces (all-zero included), and anything
/// a safe setter on [`T::Mut`](CCell::Mut) then writes into it.
///
/// That makes the setters part of this contract. A setter that can leave the
/// value in a state `c_dispose` cannot handle — an arbitrary pointer in a field
/// it frees, a length longer than the buffer it clears — must validate its
/// input or be `unsafe`, or a safe `CVal` would hand that state to the
/// disposal routine on drop.
pub unsafe trait CDispose<T> {
    /// Dispose the resources of the value at `ptr`.
    ///
    /// # Safety
    ///
    /// `ptr` must address a live, initialised `T` the caller owns.
    unsafe fn c_dispose(&self, ptr: NonNull<T>);
}

// ===========================================================================
// Buffer elements
// ===========================================================================

/// Element types a [`CVec`] may materialise as a slice: a plain Rust value,
/// every bit pattern of which is valid.
///
/// [`CVec::as_slice`] hands out `&[Self]`, which asserts well-formedness for
/// all `count` elements at once. This marker carries that claim, checked once
/// at the type rather than at each view.
///
/// | Category | Why it holds |
/// |---|---|
/// | integers, floats, raw pointers | no invalid bit patterns |
/// | [`MaybeUninit<T>`](core::mem::MaybeUninit) | valid for any bits — the escape hatch for a buffer C has not filled |
/// | arrays of the above, `()` | element-wise |
///
/// **A wrapped C type is not one.** A layout newtype from
/// [`define_ctype!`](crate::define_ctype) implements [`CCell`],
/// not this, so `&[Foo]` — which would be a reference covering C objects — does
/// not typecheck. Iterate a buffer of those with
/// [`CVec::as_handles`] instead.
///
/// `bool` and `char` are also excluded: C's `_Bool` may hold a byte outside
/// `{0, 1}` and a `char` outside the Unicode scalar range, both invalid Rust
/// values. Use the integer type and convert.
///
/// # Safety
///
/// Every bit pattern that may appear in a `CVec<Self, _>`'s buffer must be a
/// valid `Self`. The slice reference is formed over the whole buffer at once, so
/// a single bad element is undefined behaviour for the entire borrow.
pub unsafe trait CElem {}

macro_rules! impl_celem_for_primitives {
    ($($t:ty),* $(,)?) => {$(
        // SAFETY: no bit pattern of this type is invalid.
        unsafe impl CElem for $t {}
    )*};
}

impl_celem_for_primitives!(
    u8, u16, u32, u64, u128, usize, i8, i16, i32, i64, i128, isize, f32, f64,
);

// SAFETY: `MaybeUninit<T>` is valid for every bit pattern, which is the whole
// point of it — the standard escape hatch for a buffer C has not filled.
unsafe impl<T> CElem for core::mem::MaybeUninit<T> {}

// SAFETY: a raw pointer is valid for every bit pattern, null included.
unsafe impl<T: ?Sized> CElem for *const T {}
// SAFETY: as above.
unsafe impl<T: ?Sized> CElem for *mut T {}

// SAFETY: an array of valid elements is valid.
unsafe impl<T: CElem, const N: usize> CElem for [T; N] {}
// SAFETY: a ZST has one bit pattern, the empty one.
unsafe impl CElem for () {}

// ===========================================================================
// Length-aware buffer policies — drive CVec's cleanup / clone
// ===========================================================================

/// Byte-buffer cleanup policy; the bound [`CVec<T, S>`] drops on. Like
/// [`CDrop`], implemented on a policy (typically a ZST), not on the element
/// type, so one element type pairs with several policies — plain free, secure
/// zero-then-free, zero-only — at zero runtime cost.
///
/// # Safety
///
/// `c_drop_len` must release the buffer at `ptr` exactly once, under whatever
/// allocator and cleanup policy `self` represents, touching at most `byte_len`
/// bytes of it, and must be sound for every `byte_len` its method contract
/// admits.
///
/// # Example
///
/// ```ignore
/// pub struct SecureFree;
/// unsafe impl CLenDrop for SecureFree {
///     unsafe fn c_drop_len(&self, ptr: *mut u8, byte_len: usize) {
///         unsafe {
///             explicit_bzero(ptr.cast(), byte_len);
///             libc::free(ptr.cast());
///         }
///     }
/// }
/// ```
pub unsafe trait CLenDrop {
    /// Free the buffer at `ptr`, whose first `byte_len` bytes are in use.
    ///
    /// # Safety
    ///
    /// - `ptr` must be a live allocation the caller owns, from the allocator
    ///   this policy targets.
    /// - `byte_len` must not exceed the allocation's size, and must equal it
    ///   exactly when the policy's free takes a size (`OPENSSL_clear_free`, a
    ///   sized deallocator) — passing less there frees the wrong amount, or
    ///   leaves the tail uncleared.
    unsafe fn c_drop_len(&self, ptr: *mut u8, byte_len: usize);
}

/// Deep-copy policy for a length-aware buffer (a `memdup`): the length-aware
/// analogue of [`CDupClone`], needed because a buffer copy carries a byte length
/// that a pointer-only `c_dup` cannot. Gives [`CVec<T, S>`] its [`Clone`],
/// and only on opt-in — a `CLenDrop`-only policy is deliberately not cloneable.
///
/// This policy copies bytes, not elements. [`CVec`] therefore exposes
/// cloning only when `T: Copy`; a buffer of owning elements needs a
/// per-element clone contract, which this trait does not provide.
///
/// # Safety
///
/// `c_clone_len` must return a fresh, uniquely-owned allocation of `byte_len`
/// bytes byte-copied from `ptr` and releasable by this policy's [`CLenDrop`]
/// impl — or `None` on allocation failure. It must not invalidate `ptr`.
pub unsafe trait CLenClone: CLenDrop {
    /// Byte-copy the `byte_len`-byte buffer at `ptr` into a fresh allocation,
    /// or `None` on failure.
    ///
    /// # Safety
    ///
    /// `ptr` must point to a live allocation of at least `byte_len` bytes
    /// compatible with this policy's allocator.
    unsafe fn c_clone_len(&self, ptr: *mut u8, byte_len: usize) -> Option<NonNull<u8>>;
}

// ---------------------------------------------------------------------------
// CCell — the link from a layout newtype to its handles
// ---------------------------------------------------------------------------

/// A `#[repr(transparent)]` newtype over a C type (`Self::C`), together with
/// the borrowed handles that carry its accessors.
///
/// A **linking** trait, not an access trait: it names the wrapped C type and
/// the two handle types, and has no methods — nothing on it is callable, so
/// implementing it exposes no constructor. ffibox builds the handles from a
/// pointer itself, relying on the layout the contract below guarantees. The seam (`as_ptr` / `as_mut_ptr` /
/// `from_ptr`) lives on the handles as inherent methods, because that is where
/// a `&self` receiver is sound — `&FooRef` covers one pointer of Rust stack
/// where `&Foo` would cover the C object.
///
/// Implemented by [`define_ctype!`](crate::define_ctype) for the trivial base
/// case, or by hand for lifetime- / type-generic newtypes.
///
/// # Safety
///
/// - `Self` MUST be layout-compatible with `Self::C`: `#[repr(transparent)]`
///   over it, any other fields zero-sized.
/// - [`Ref<'a>`](CCell::Ref) and [`Mut<'a>`](CCell::Mut) MUST each be
///   `#[repr(transparent)]` over
///   [`CBorrowedPtr<'a, Self>`](crate::CBorrowedPtr) — directly, or `Mut` over
///   `Ref` — with every other field zero-sized and no `Drop`: ffibox builds
///   them from a `CBorrowedPtr` by layout alone. They borrow the pointee for
///   `'a` and MUST NOT hand out a reference to `Self`.
/// - `Ref` MUST NOT offer any operation that writes through the pointer — that
///   is `Mut`'s job, and the split is what keeps a shared borrow shared.
/// - `Mut<'a>` MUST be invariant in `Self`, as `&'a mut Self` is. A
///   [`CBorrowedPtr`](crate::CBorrowedPtr) alone is covariant, like `&'a T`, so
///   add a `PhantomData<&'a mut Self>` field. This matters only when `Self`
///   has lifetime or type parameters (what [`define_ctype!`](crate::define_ctype)
///   emits has none, and carries the marker anyway): without it,
///   `Mut<'a>` over `Holder<'static>` would coerce to one over
///   `Holder<'short>`, whose setter could store a `'short` borrow that the
///   `'static` owner later reads.
///
/// ```compile_fail
/// use core::marker::PhantomData;
/// use ffibox::{CBorrowedPtr, CCell};
///
/// #[repr(C)]
/// pub struct holder_st { p: *const u32 }
/// /// Holds a `&'x u32` in a C field.
/// #[repr(transparent)]
/// pub struct Holder<'x>(holder_st, PhantomData<&'x u32>);
/// #[repr(transparent)]
/// #[derive(Clone, Copy)]
/// pub struct HolderRef<'a, 'x>(CBorrowedPtr<'a, Holder<'x>>);
/// #[repr(transparent)]
/// pub struct HolderMut<'a, 'x>(HolderRef<'a, 'x>, PhantomData<&'a mut Holder<'x>>);
///
/// unsafe impl<'x> CCell for Holder<'x> {
///     type C = holder_st;
///     type Ref<'a> = HolderRef<'a, 'x> where Self: 'a;
///     type Mut<'a> = HolderMut<'a, 'x> where Self: 'a;
/// }
///
/// // Rejected thanks to the marker; without it, this compiles.
/// fn shrink<'a, 's>(m: HolderMut<'a, 'static>) -> HolderMut<'a, 's> {
///     m
/// }
/// ```
///
/// The handle size is checked at compile time wherever ffibox builds a handle,
/// so an impl whose `Ref` or `Mut` is not pointer-sized fails the build:
///
/// ```compile_fail,E0080
/// use core::ptr::NonNull;
/// use ffibox::{CBorrowedPtr, CCell, CSlice};
///
/// #[repr(C)]
/// pub struct raw_st { x: u8 }
/// #[repr(transparent)]
/// pub struct Foo(raw_st);
/// #[derive(Clone, Copy)]
/// pub struct FooRef<'a>(CBorrowedPtr<'a, Foo>, u64); // not transparent
/// pub struct FooMut<'a>(CBorrowedPtr<'a, Foo>);
///
/// unsafe impl CCell for Foo {
///     type C = raw_st;
///     type Ref<'a> = FooRef<'a>;
///     type Mut<'a> = FooMut<'a>;
/// }
///
/// let mut v = Foo(raw_st { x: 0 });
/// let run = unsafe { CSlice::from_raw_parts(NonNull::from(&mut v), 1) };
/// let _ = run.get(0); // instantiates the handle conversion for `Foo`
/// ```
pub unsafe trait CCell: Sized {
    /// The wrapped C FFI type (e.g. `ffi::stack_st`).
    type C;

    /// The shared borrowed handle — `Copy`, getters only.
    ///
    /// `Self: 'a` because a handle borrows the object: a generic wrapper's
    /// parameters must outlive the borrow, exactly as `&'a T` requires.
    type Ref<'a>: Copy
    where
        Self: 'a;

    /// The exclusive borrowed handle — move-only, getters plus setters.
    type Mut<'a>
    where
        Self: 'a;
}
