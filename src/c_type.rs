//! [`CType<T>`] — the canonical safe wrapper for types from `*-sys` crates.
//!
//! Bindgen-generated `#[repr(C)]` structs cross the FFI boundary: C may
//! initialise them in place, mutate them through raw pointers, and thread them
//! into intrusive lists. None of that is sound under Rust's default
//! assumptions. `CType<T>` = `UnsafeCell<MaybeUninit<T>>` + `PhantomPinned`,
//! one annotation per problem:
//!
//! - **`UnsafeCell`** — drops the `noalias` / `readonly` attributes Rust would
//!   emit on `&CType<T>`. Without it LLVM may hoist field reads across a C call
//!   that writes the same address through a raw pointer.
//! - **`MaybeUninit`** — suppresses type-validity invariants (enum
//!   discriminants, non-null pointers, `bool ∈ {0,1}`) that C has not yet
//!   established when it initialises in place.
//! - **`PhantomPinned`** — makes the type `!Unpin`, so Rust cannot move a
//!   struct that C-side list pointers already reference.
//!
//! # Scope of `UnsafeCell` protection
//!
//! `UnsafeCell` suppresses `noalias` / `readonly` only on references to the
//! type that *contains* it — `&CType<T>`. It does not propagate through the
//! `*mut T` from [`get()`](CType::get): `&(*raw).field` yields a naked
//! `&FieldType` carrying the full guarantees, because the field itself is not
//! wrapped. Concurrent C mutation through another pointer would then be UB.
//!
//! So the model is two layers, both required:
//!
//! | Layer | Mechanism | Protects |
//! |-------|-----------|----------|
//! | Wrapper reference | `UnsafeCell` (this type) | holding `&CType<T>` while C mutates the same memory |
//! | Field projection  | `addr_of!` / `addr_of_mut!` discipline | field access never forms a Rust reference to a field of the inner `T` |
//!
//! **Corollary:** never implement `Deref<Target = T>` on a wrapper over
//! `CType<T>` — `&T` carries the guarantees layer 2 exists to avoid. Project
//! per field through [`get()`](CType::get) instead.
//!
//! # Layout
//!
//! `#[repr(transparent)]` over `UnsafeCell<MaybeUninit<T>>`; `PhantomPinned` is
//! a ZST. Hence `size_of::<CType<T>>() == size_of::<T>()` and likewise for
//! alignment, and `*mut T` / `*mut CType<T>` address the same memory —
//! [`cast_into`](CType::cast_into) / [`cast_from`](CType::cast_from) make the
//! conversion explicit.
//!
//! # Relationship to `define_type!`
//!
//! [`define_type!`](crate::define_type) emits a `#[repr(transparent)]` newtype
//! over `CType<T>`, giving each C struct a distinct Rust type while inheriting
//! this layout:
//!
//! ```ignore
//! define_type!(SslSession, libssl_sys::ssl_session_st);
//! // → #[repr(transparent)] pub struct SslSession(CType<libssl_sys::ssl_session_st>);
//! ```
//!
//! RFL's `Opaque<T>` is the same design; the name differs because here
//! [`get()`](CType::get) deliberately grants field access.
//!
//! # Construction-phase ownership
//!
//! `CType` suppresses type-level validity only. The allocation lifecycle lives
//! one layer up, at the smart pointer:
//!
//! | Allocation site | Pre-init handle                       | Post-init handle      |
//! |-----------------|---------------------------------------|-----------------------|
//! | Stack           | [`CType::uninit`] / [`CType::zeroed`]  | `&CType<T>`           |
//! | Heap (C-side)   | [`CBoxUninit`](crate::CBoxUninit)      | [`CBox`](crate::CBox) |
//!
//! On the heap path: allocate with the C allocator, wrap in `CBoxUninit`,
//! initialise fields — *including* any C refcount — through raw projections,
//! then `assume_init`. During that window `Drop` must not run the real
//! destructor (a down-ref would read an uninitialised refcount), which is why
//! `CBoxUninit` runs only
//! [`CDroppedUninit::c_drop_uninit`](crate::CDroppedUninit::c_drop_uninit).
//! Whether the resulting `CBox` is a sole owner or one refcount share is
//! decided by the registered `CDropped` / `CCloned` routines, not by this path.
//!
//! # Examples
//!
//! ```ignore
//! // Stack: allocate uninit, let C initialise in place.
//! let mut pkt = CType::<ffi::WPACKET>::uninit();
//! unsafe { ffi::WPACKET_init_der(pkt.get(), buf.as_mut_ptr(), buf.len()) };
//!
//! // Heap, already initialised by C: borrow it as the newtype.
//! let ctx: &SslCtx = unsafe { SslCtx::from_ptr(ffi::SSL_CTX_new(method)) }.unwrap();
//!
//! // Heap, initialised by us: uninit slot → field writes → promote.
//! let raw = unsafe { ffi::CRYPTO_malloc(SIZE, FILE, LINE) } as *mut SslSession;
//! let mut slot =
//!     unsafe { CBoxUninit::<SslSession>::from_raw_uninit(raw.cast()) }.unwrap();
//! unsafe {
//!     let p = slot.as_mut_ptr();
//!     ffi::ossl_CRYPTO_NEW_REF(&mut (*p.cast::<ffi::ssl_session_st>()).references, 1);
//! }
//! let session: CBox<SslSession> = unsafe { slot.assume_init() };
//! ```

use core::cell::UnsafeCell;
use core::marker::PhantomPinned;
use core::mem::MaybeUninit;

/// Safe wrapper for types from `*-sys` crates: [`UnsafeCell`] (interior
/// mutability) + [`MaybeUninit`] (uninit-capable) + [`PhantomPinned`]
/// (`!Unpin`). See the [module docs](self) for why each is required.
#[repr(transparent)]
pub struct CType<T> {
    value: UnsafeCell<MaybeUninit<T>>,
    _pin: PhantomPinned,
}

impl<T> CType<T> {
    /// Wrap an already-initialised value. For types a C constructor
    /// initialises in place, prefer [`uninit`](Self::uninit).
    #[inline]
    pub const fn new(value: T) -> Self {
        Self {
            value: UnsafeCell::new(MaybeUninit::new(value)),
            _pin: PhantomPinned,
        }
    }

    /// Stack-allocate an uninitialised `CType<T>`. A C constructor must
    /// initialise it before any field is read through [`get`](Self::get).
    ///
    /// The heap counterpart is [`CBoxUninit`](crate::CBoxUninit).
    #[inline]
    pub const fn uninit() -> Self {
        Self {
            value: UnsafeCell::new(MaybeUninit::uninit()),
            _pin: PhantomPinned,
        }
    }

    /// Zero-initialise (C's `memset(p, 0, sizeof)`). Correct only where
    /// all-zero bytes are a valid initial state for `T`.
    ///
    /// Not `const fn`: [`MaybeUninit::zeroed`] became `const` in Rust 1.75,
    /// and this crate targets 1.70+.
    #[inline]
    pub fn zeroed() -> Self {
        Self {
            value: UnsafeCell::new(MaybeUninit::zeroed()),
            _pin: PhantomPinned,
        }
    }

    /// Raw pointer to the inner `T`, valid for the lifetime of `self` — the
    /// primary access path, passed straight to any C function taking `*mut T`.
    /// The caller must ensure `T` is initialised before reading a field.
    #[inline]
    pub const fn get(&self) -> *mut T {
        // *mut UnsafeCell<MaybeUninit<T>> → *mut T; both are transparent, so
        // the address is unchanged.
        UnsafeCell::get(&self.value).cast::<T>()
    }

    /// Cast `*const CType<T>` to `*mut T` — [`get`](Self::get) for when you
    /// hold a pointer rather than a reference.
    #[inline]
    pub const fn cast_into(this: *const Self) -> *mut T {
        // repr(transparent) throughout, so `raw_get` + a cast preserves the
        // address.
        UnsafeCell::raw_get(this.cast::<UnsafeCell<MaybeUninit<T>>>()).cast::<T>()
    }

    /// Cast `*const T` to `*const CType<T>` — the reverse of
    /// [`cast_into`](Self::cast_into), valid by the shared layout.
    #[inline]
    pub const fn cast_from(this: *const T) -> *const Self {
        this.cast::<Self>()
    }
}

/// A `#[repr(transparent)]` newtype over [`CType<Self::C>`](CType) (hence over
/// `UnsafeCell<MaybeUninit<Self::C>>`), where `C` names the wrapped C FFI type.
///
/// Implemented with **only `type C = …;`** — by [`define_type!`](crate::define_type)
/// for the trivial base case, or by a hand-written `unsafe impl` for lifetime /
/// type-generic newtypes (native Rust generics, no macro). The raw-pointer seam
/// (`as_ptr` / `as_void_ptr` / `from_ptr` / `from_void_ptr`) and the stack
/// constructors (`uninit` / `zeroed`) are all provided.
///
/// Generic `void*` shims (`memcpy`, `memset`, …) bound on `W: CCell` and pass
/// [`as_void_ptr`](CCell::as_void_ptr): the `UnsafeCell` storage lets a shared
/// `&self` alias memory C still writes, which a `&mut` could not.
///
/// # Safety
///
/// An implementor MUST be `#[repr(transparent)]` over `CType<Self::C>`. Every
/// provided method relies on that layout equality; a wrong `#[repr]` is
/// undefined behaviour. `define_type!` guarantees it, a hand-written impl
/// asserts it.
pub unsafe trait CCell: Sized {
    /// The wrapped C FFI type (e.g. `ffi::stack_st`).
    type C;

    /// Raw pointer to the stored C object, for FFI calls and field projection.
    /// Routes through [`CType::cast_into`] (`UnsafeCell::raw_get`), so it is
    /// sound to read **and write** through even from a shared `&self`. Never
    /// form a `&Self::C` / `&mut Self::C` to the raw struct.
    #[inline]
    fn as_ptr(&self) -> *mut Self::C {
        // SAFETY: `Self` is `#[repr(transparent)]` over `CType<Self::C>`, so
        // `*const Self` reinterprets as `*const CType<Self::C>`.
        CType::<Self::C>::cast_into((self as *const Self).cast())
    }

    /// Type-erased `void*` to the stored C object, for generic `void*` shims
    /// (`memcpy`, `memset`, …). Byte length is `size_of::<Self::C>()`.
    #[inline]
    fn as_void_ptr(&self) -> *mut core::ffi::c_void {
        self.as_ptr().cast()
    }

    /// Borrow a raw pointer as a **shared** reference; `None` if null.
    ///
    /// Shared only: a `&mut Self` would assert a `noalias` the FFI seam cannot
    /// honour, since C keeps its own alias. Mutate through `&self` +
    /// [`as_ptr`](CCell::as_ptr) + `addr_of_mut!` instead.
    ///
    /// # Safety
    ///
    /// `ptr` must point to a valid, initialised `Self::C` (or be null); the
    /// returned reference must not outlive the object.
    #[inline]
    unsafe fn from_ptr<'a>(ptr: *mut Self::C) -> Option<&'a Self> {
        // SAFETY: `*mut Self::C` → `*const Self` is layout-preserving by the
        // repr-transparent contract; `as_ref` handles null.
        unsafe { ptr.cast::<Self>().as_ref() }
    }

    /// Borrow a type-erased `void*` as a **shared** reference; `None` if null.
    /// The inbound dual of [`as_void_ptr`](CCell::as_void_ptr), for C slots
    /// that hand an opaque pointer back (`SSL_get_ex_data`, `BIO_get_data`, a
    /// callback's `void *arg`). Ownership is not transferred: the slot keeps
    /// whatever teardown obligation it held.
    ///
    /// # Safety
    ///
    /// As [`from_ptr`](CCell::from_ptr), plus: `ptr` must be the pointer erased
    /// *from a `Self`*. Nothing in a `void*` records the type, so one erased
    /// from another type reconstitutes as type confusion.
    #[inline]
    unsafe fn from_void_ptr<'a>(ptr: *mut core::ffi::c_void) -> Option<&'a Self> {
        // SAFETY: the caller asserts `ptr` addresses a live `Self::C`, so the
        // cast restores the type it was erased from; `from_ptr` handles null.
        unsafe { Self::from_ptr(ptr.cast::<Self::C>()) }
    }

    /// Stack-allocate an **uninitialised** wrapper. Initialise via a C
    /// constructor (`self.as_ptr()`) before reading any field.
    #[inline]
    fn uninit() -> Self {
        // SAFETY: `Self` is `#[repr(transparent)]` over `CType<Self::C>`, so
        // byte-identical to one, and the cell is `MaybeUninit` — valid for any
        // bit pattern. `transmute_copy` because the size equality is a generic
        // contract, not statically known.
        unsafe { core::mem::transmute_copy(&CType::<Self::C>::uninit()) }
    }

    /// Zero-initialise a wrapper. Valid only where all-zero bytes are a valid
    /// initial state for `Self::C`.
    #[inline]
    fn zeroed() -> Self {
        // SAFETY: as `uninit`, with a zeroed cell.
        unsafe { core::mem::transmute_copy(&CType::<Self::C>::zeroed()) }
    }
}

// `CType<T>` is trivially `#[repr(transparent)]` over itself, so it is its own
// `CCell`. This serves type-erased storage a wrapper holds directly, e.g. a
// `CType<c_void>` callback-payload field handed to C via `as_void_ptr()`.
// SAFETY: `CType<T>` is `#[repr(transparent)]` over itself.
unsafe impl<T> CCell for CType<T> {
    type C = T;
}
