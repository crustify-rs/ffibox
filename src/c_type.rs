//! [`CType<T>`] — the layout newtype for a `*-sys` type — and [`CCell`], the
//! trait linking it to its borrowed handles.
//!
//! # Nothing ever references a wrapped C object
//!
//! That is the one rule this crate is built on; everything else follows. A
//! `&Wrapper` covering a C object's bytes asserts `noalias` / `readonly` /
//! validity over memory C may write through a pointer it kept — so no such
//! reference is ever formed. Access goes through **handles** that hold the
//! pointer by value:
//!
//! | Type | Size | Role |
//! |------|------|------|
//! | `Foo` (= [`CType<ffi::foo>`](CType)) | the C struct's | layout only: embeds by value in a `#[repr(C)]` mirror, and is what [`CBox`](crate::CBox) points at. **Never referenced.** |
//! | `FooRef<'a>` | one pointer | shared borrow; `Copy`; the getters live here |
//! | `FooMut<'a>` | one pointer | exclusive borrow; derefs to `FooRef<'a>`, adds the setters |
//!
//! `&FooRef` covers the *handle* — one pointer of Rust-owned stack — never the
//! C object, so it carries no claim about it. Field access projects a raw
//! pointer out of the handle and reads or writes through `addr_of!` /
//! `addr_of_mut!`, which form no reference either.
//!
//! That is why the handles get `&self` / `&mut self` methods for free, and why
//! passing `&mut FooRef` around reborrows implicitly the way `&mut T` does.
//!
//! # What `CType` still carries
//!
//! Only `PhantomPinned`. Address-sensitivity is **independent** of aliasing: a
//! self-referential struct, or one C recorded in a list, must not move whether
//! or not anyone holds a reference to it. So `CType<T>` is `T` plus `!Unpin`
//! and nothing else — no `UnsafeCell` (no reference to strip attributes from)
//! and no `MaybeUninit` (no reference asserting validity).
//!
//! # Layout
//!
//! `#[repr(transparent)]` over `T`, and `PhantomPinned` is a ZST, so
//! `size_of::<CType<T>>() == size_of::<T>()` and likewise for alignment. A
//! generated newtype is transparent over `CType<T>` in turn, so `Foo` embeds by
//! value in a `#[repr(C)]` parent and `Option<CBox<Foo>>` is a niche
//! `*mut ffi::foo`.
//!
//! # Relationship to the macro
//!
//! [`define_ctype!`](crate::define_ctype) emits all three types at once,
//! together with the [`CCell`] impl that links them:
//!
//! ```ignore
//! define_ctype!(SslSession, libssl_sys::ssl_session_st);
//! // -> pub struct SslSession(CType<libssl_sys::ssl_session_st>);
//! //    pub struct SslSessionRef<'a>(..);   // getters
//! //    pub struct SslSessionMut<'a>(..);   // + setters
//! //    unsafe impl CCell for SslSession { type C = ..; type Ref<'a> = ..; type Mut<'a> = ..; }
//! ```

use core::marker::{PhantomData, PhantomPinned};
use core::ptr::NonNull;

// ---------------------------------------------------------------------------
// CType<T> — the layout newtype
// ---------------------------------------------------------------------------

/// Layout newtype for a type from a `*-sys` crate: `T` plus [`PhantomPinned`].
///
/// It gives each C struct a distinct, address-pinned Rust type that keeps the C
/// layout — for embedding by value in a `#[repr(C)]` mirror, and as the pointee
/// of [`CBox`](crate::CBox) and the borrowed handles.
///
/// **A reference to one is never formed.** Every accessor lives on the handles
/// (see the [module docs](self)), which hold the pointer by value. The
/// constructors here produce a value for inline storage ([`CVal`](crate::CVal))
/// or for a stack slot a C routine will initialise; the pointer to it is taken
/// with `addr_of_mut!`, never `&mut`.
#[repr(transparent)]
pub struct CType<T> {
    value: T,
    _pin: PhantomPinned,
}

impl<T> CType<T> {
    /// Wrap an already-initialised value.
    #[inline]
    pub const fn new(value: T) -> Self {
        Self {
            value,
            _pin: PhantomPinned,
        }
    }

    /// Zero-initialise (C's `memset(p, 0, sizeof)`).
    ///
    /// # Safety
    ///
    /// The all-zero bit pattern must be a valid `T`. It is for every bindgen
    /// `#[repr(C)]` struct over a C header — C has no niche types, and bindgen
    /// emits enums as integer constants — but *not* for an arbitrary Rust `T`
    /// (`NonNull`, `&U`, a `#[repr(Rust)]` enum without a zero variant).
    #[inline]
    pub const unsafe fn zeroed() -> Self {
        Self {
            // SAFETY: the caller asserts all-zero is a valid bit pattern for `T`.
            value: unsafe { core::mem::MaybeUninit::zeroed().assume_init() },
            _pin: PhantomPinned,
        }
    }

    /// Read-only pointer to the inner `T`.
    ///
    /// Takes a raw pointer rather than `&self`, because `&CType<T>` is the
    /// reference this crate never forms. Reach a value held inline with
    /// `addr_of!(slot)`.
    #[inline]
    pub const fn cast_into(this: *const Self) -> *const T {
        this.cast::<T>()
    }

    /// Writable pointer to the inner `T` — the counterpart of
    /// [`cast_into`](Self::cast_into), for a pointer carrying write provenance
    /// (`addr_of_mut!(slot)`).
    #[inline]
    pub const fn cast_into_mut(this: *mut Self) -> *mut T {
        this.cast::<T>()
    }

    /// Cast `*const T` to `*const CType<T>`, valid by the shared layout.
    #[inline]
    pub const fn cast_from(this: *const T) -> *const Self {
        this.cast::<Self>()
    }
}

// ---------------------------------------------------------------------------
// CPtr — the storage behind every generated handle
// ---------------------------------------------------------------------------

/// The storage every generated `FooRef<'a>` / `FooMut<'a>` is transparent
/// over: one pointer, tagged with the borrow's lifetime.
///
/// Covariant in `'a` (a longer borrow coerces to a shorter one) and `Copy`,
/// exactly like `&'a T`. `Option<CPtr<'a, T>>` is a niche `*const T`, so it
/// substitutes for a raw pointer at the FFI seam.
#[repr(transparent)]
pub struct CPtr<'a, T> {
    ptr: NonNull<T>,
    _borrow: PhantomData<&'a T>,
}

impl<T> Clone for CPtr<'_, T> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}
// A shared handle copies like `&T`; `#[derive]` would add a spurious `T: Copy`.
impl<T> Copy for CPtr<'_, T> {}

impl<'a, T> CPtr<'a, T> {
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

// ---------------------------------------------------------------------------
// CCell — the link from a layout newtype to its handles
// ---------------------------------------------------------------------------

/// A `#[repr(transparent)]` newtype over [`CType<Self::C>`](CType), together
/// with the borrowed handles that carry its accessors.
///
/// A **linking** trait, not an access trait: it names the wrapped C type and
/// the two handle types, and nothing else. The seam (`as_ptr` / `as_mut_ptr` /
/// `from_ptr`) lives on the handles as inherent methods, because that is where
/// a `&self` receiver is sound — `&FooRef` covers one pointer of Rust stack
/// where `&Foo` would cover the C object.
///
/// Implemented by [`define_ctype!`](crate::define_ctype) for the trivial base
/// case, or by hand for lifetime- / type-generic newtypes.
///
/// # Safety
///
/// - `Self` MUST be `#[repr(transparent)]` over `CType<Self::C>`.
/// - [`Ref`](CCell::Ref) and [`Mut`](CCell::Mut) MUST be handles over
///   `CPtr<'a, Self>` that borrow the pointee for `'a`, and MUST NOT hand out a
///   reference to `Self`.
/// - `Ref` MUST NOT offer any operation that writes through the pointer — that
///   is `Mut`'s job, and the split is what keeps a shared borrow shared.
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

    /// Build a shared handle from a pointer to the wrapper.
    ///
    /// # Safety
    ///
    /// `p` must address a live, initialised `Self::C` that outlives `'a`.
    unsafe fn ref_from_raw<'a>(p: NonNull<Self>) -> Self::Ref<'a>
    where
        Self: 'a;

    /// Build an exclusive handle from a pointer to the wrapper.
    ///
    /// # Safety
    ///
    /// As [`ref_from_raw`](CCell::ref_from_raw), plus: no other handle to the
    /// same object may be used while the result lives.
    unsafe fn mut_from_raw<'a>(p: NonNull<Self>) -> Self::Mut<'a>
    where
        Self: 'a;
}
