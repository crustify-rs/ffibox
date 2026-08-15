//! Declarative macros defining safe wrapper types and registering lifecycle
//! traits on them.
//!
//! [`define_ctype!`](crate::define_ctype) emits the `#[repr(transparent)]`
//! newtype; the `impl_*!` macros bind a C routine to the matching trait:
//! [`impl_dropped!`](crate::impl_dropped) (a `*_free` or a down-ref),
//! [`impl_cloned!`](crate::impl_cloned) (a `*_dup` or an `up_ref`, named at the
//! call site), and [`impl_cvalued!`](crate::impl_cvalued) (a by-value dispose).
//! The construction-phase storage free has no macro: write it as a ZST
//! [`CDropper`](crate::CDropper) and hold the allocation in a
//! [`CBoxWith`](crate::CBoxWith) until [`into_box`](crate::CBoxWith::into_box).

/// Define a wrapped `*-sys` type: the layout newtype plus its two borrowed
/// handles.
///
/// ```ignore
/// define_ctype!(SslSession, SslSessionRef, SslSessionMut, ffi::ssl_session_st);
/// ```
///
/// All three names are spelled out because `macro_rules!` cannot concatenate
/// identifiers; the order is layout, shared, exclusive.
///
/// # What it emits
///
/// | Item | Shape | Carries |
/// |------|-------|---------|
/// | `$name` | `#[repr(transparent)]` over `CType<$c_type>` | the C layout — embeds by value in a `#[repr(C)]` mirror, and is what `CBox` points at |
/// | `$rf<'a>` | `#[repr(transparent)]` over `CPtr<'a, $name>`, `Copy` | the shared seam and **all getters** |
/// | `$mt<'a>` | `#[repr(transparent)]` over `$rf<'a>` | `Deref` to `$rf`, plus the write seam and **all setters** |
///
/// plus the [`CCell`](crate::CCell) impl linking them.
///
/// # Where accessors go
///
/// Getters on `$rf<'a>` taking `&self`, setters on `$mt<'a>` taking
/// `&mut self`. Both project a raw pointer out of the handle:
///
/// ```ignore
/// impl SslSessionRef<'_> {
///     pub fn timeout(&self) -> u64 {
///         // SAFETY: read through the raw pointer; no reference to the C
///         // object is formed, so nothing is asserted about its bytes.
///         unsafe { core::ptr::addr_of!((*self.as_ptr()).timeout).read() }
///     }
/// }
/// impl SslSessionMut<'_> {
///     pub fn set_timeout(&mut self, v: u64) {
///         unsafe { core::ptr::addr_of_mut!((*self.as_mut_ptr()).timeout).write(v) }
///     }
/// }
/// ```
///
/// **Never write an accessor on `$name` itself**, and never take `&$name` /
/// `&mut $name`: those cover the C object's bytes and would assert `noalias` /
/// `readonly` / validity over memory C may write. The handles cover one pointer
/// of Rust stack instead. See the [`c_type`](crate::c_type) module docs.
///
/// # Safety
///
/// The macro is safe to invoke but emits an `unsafe impl`. You assert that
/// `$c_type` is the C type `$name` mirrors, and that all-zero is a valid
/// `$c_type` (see `zeroed` below).
#[macro_export]
macro_rules! define_ctype {
    ($(#[$attr:meta])* $name:ident, $rf:ident, $mt:ident, $c_type:ty) => {
        $(#[$attr])*
        #[repr(transparent)]
        pub struct $name($crate::c_type::CType<$c_type>);

        #[doc = concat!("Shared borrow of a [`", stringify!($name), "`]. `Copy`, like `&T`; the getters live here.")]
        #[repr(transparent)]
        #[derive(Clone, Copy)]
        pub struct $rf<'a>($crate::c_type::CPtr<'a, $name>);

        #[doc = concat!("Exclusive borrow of a [`", stringify!($name), "`]. Derefs to [`", stringify!($rf), "`] for the getters and adds the setters.")]
        #[repr(transparent)]
        pub struct $mt<'a>($rf<'a>);

        // SAFETY: `$name` is `#[repr(transparent)]` over `CType<$c_type>`; the
        // handles are transparent over `CPtr<'a, $name>` and expose no
        // reference to `$name`; `$rf` has no write operation.
        unsafe impl $crate::c_type::CCell for $name {
            type C = $c_type;
            type Ref<'a> = $rf<'a>;
            type Mut<'a> = $mt<'a>;

            #[inline]
            unsafe fn ref_from_raw<'a>(p: ::core::ptr::NonNull<Self>) -> $rf<'a> {
                // SAFETY: caller upholds `ref_from_raw`'s contract.
                $rf(unsafe { $crate::c_type::CPtr::new(p) })
            }

            #[inline]
            unsafe fn mut_from_raw<'a>(p: ::core::ptr::NonNull<Self>) -> $mt<'a> {
                // SAFETY: caller upholds `mut_from_raw`'s contract.
                $mt($rf(unsafe { $crate::c_type::CPtr::new(p) }))
            }
        }

        impl $name {
            /// Zero-initialise a value for inline storage
            /// ([`CVal`](crate::CVal)) or a stack slot.
            ///
            /// Valid because `$c_type` is a bindgen `#[repr(C)]` struct over a
            /// C header, which has no niche types — asserted by invoking
            /// [`define_ctype!`](crate::define_ctype) on it.
            #[inline]
            #[must_use]
            pub fn zeroed() -> Self {
                // SAFETY: all-zero is a valid bit pattern for a bindgen C
                // struct; see `define_ctype!`.
                Self(unsafe { $crate::c_type::CType::zeroed() })
            }
        }

        impl<'a> $rf<'a> {
            /// Borrow a raw pointer; `None` if null.
            ///
            /// # Safety
            ///
            /// `ptr` must address a live, initialised `$c_type` (or be null)
            /// that outlives `'a`.
            #[inline]
            pub unsafe fn from_ptr(ptr: *mut $c_type) -> ::core::option::Option<Self> {
                // SAFETY: layout-preserving cast per `#[repr(transparent)]`;
                // the caller upholds liveness and the lifetime.
                ::core::ptr::NonNull::new(ptr.cast::<$name>())
                    .map(|p| $rf(unsafe { $crate::c_type::CPtr::new(p) }))
            }

            /// Read-only pointer to the C object, for FFI calls taking
            /// `*const $c_type` and for field reads through `addr_of!`.
            #[inline]
            #[must_use]
            pub fn as_ptr(&self) -> *const $c_type {
                self.0.as_non_null().as_ptr().cast::<$c_type>()
            }

            /// Type-erased `*const c_void`, for read-only `void *` shims.
            #[inline]
            #[must_use]
            pub fn as_void_ptr(&self) -> *const ::core::ffi::c_void {
                self.as_ptr().cast()
            }

            /// Borrow a type-erased `void *` back; `None` if null. The inbound
            /// dual of [`as_void_ptr`](Self::as_void_ptr), for C slots that
            /// hand an opaque pointer back (`SSL_get_ex_data`, `BIO_get_data`,
            /// a callback's `void *arg`). Ownership is not transferred.
            ///
            /// # Safety
            ///
            /// As [`from_ptr`](Self::from_ptr), plus: `ptr` must be the pointer
            /// erased *from this very type*. Nothing in a `void *` records the
            /// type, so one erased from another reconstitutes as confusion.
            #[inline]
            pub unsafe fn from_void_ptr(
                ptr: *mut ::core::ffi::c_void,
            ) -> ::core::option::Option<Self> {
                // SAFETY: the caller asserts `ptr` addresses a live `$c_type`.
                unsafe { Self::from_ptr(ptr.cast::<$c_type>()) }
            }
        }

        impl<'a> $mt<'a> {
            /// Borrow a raw pointer exclusively; `None` if null.
            ///
            /// # Safety
            ///
            /// As [`from_ptr`](Self::from_ptr), plus: no other handle to the
            /// same object may be used while the result lives.
            #[inline]
            pub unsafe fn from_ptr(ptr: *mut $c_type) -> ::core::option::Option<Self> {
                // SAFETY: caller upholds liveness, the lifetime and exclusivity.
                ::core::ptr::NonNull::new(ptr.cast::<$name>())
                    .map(|p| $mt($rf(unsafe { $crate::c_type::CPtr::new(p) })))
            }

            /// Writable pointer to the C object, for FFI calls taking
            /// `*mut $c_type` and for field writes through `addr_of_mut!`.
            #[inline]
            #[must_use]
            pub fn as_mut_ptr(&mut self) -> *mut $c_type {
                self.0 .0.as_non_null().as_ptr().cast::<$c_type>()
            }

            /// Type-erased `*mut c_void`, for writing `void *` shims.
            #[inline]
            #[must_use]
            pub fn as_mut_void_ptr(&mut self) -> *mut ::core::ffi::c_void {
                self.as_mut_ptr().cast()
            }

            /// Reborrow shared, for passing where a getter-only handle is
            /// wanted.
            #[inline]
            #[must_use]
            pub fn as_ref(&self) -> $rf<'_> {
                self.0
            }
        }

        impl<'a> ::core::ops::Deref for $mt<'a> {
            type Target = $rf<'a>;
            #[inline]
            fn deref(&self) -> &$rf<'a> {
                &self.0
            }
        }
    };
}

/// Implement [`CDropped`](crate::CDropped), registering a type with
/// [`CBox<T>`](crate::CBox). Either bind a C function —
/// `impl_dropped!(EvpMdCtx, ffi::EVP_MD_CTX, ffi::EVP_MD_CTX_free)` — or, with
/// an inherent `unsafe fn free(*mut Self)`, just `impl_dropped!(MyCert)`.
///
/// Conditional teardown folds the gate into the `free` routine; see
/// [`CDropped`](crate::CDropped).
///
/// # Safety
///
/// The macro is safe to invoke but emits an `unsafe impl`. You assert that the
/// named function correctly releases all resources of a `$name`.
#[macro_export]
macro_rules! impl_dropped {
    ($name:ident) => {
        // SAFETY: caller guarantees the inherent `free` upholds the contract.
        unsafe impl $crate::traits::CDropped for $name {
            #[inline]
            unsafe fn c_drop(obj: ::core::ptr::NonNull<Self>) {
                // SAFETY: caller upholds the `c_drop` contract.
                unsafe { Self::free(obj.as_ptr()) }
            }
        }
    };
    ($name:ty, $c_type:ty, $free:path) => {
        // SAFETY: caller guarantees `$free` is the C destructor for
        // `$c_type` and `$name` is layout-compatible with `*mut $c_type`.
        unsafe impl $crate::traits::CDropped for $name {
            #[inline]
            unsafe fn c_drop(obj: ::core::ptr::NonNull<Self>) {
                // SAFETY: caller upholds the `c_drop` contract.
                unsafe { $free(obj.as_ptr() as *mut $c_type) }
            }
        }
    };
}

/// Implement [`CValued`](crate::CValued) for a **by-value** C type,
/// registering it with [`CVal<T>`](crate::CVal) — teardown that disposes owned
/// resources without freeing the header, which is Rust's inline storage.
///
/// Same two forms as [`impl_dropped!`](crate::impl_dropped):
/// `impl_cvalued!(GitOidarray, ffi::git_oidarray, ffi::git_oidarray_dispose)`,
/// or `impl_cvalued!(GitOidarray)` with an inherent
/// `unsafe fn dispose(*mut Self)`.
///
/// A type may implement both this and `CDropped` — a C library often has a
/// `*_free` (storage and fields) alongside a `*_dispose` (fields only), and the
/// wrapper you choose selects which runs. **Never register the same C function
/// under both**; that double-frees.
///
/// # Safety
///
/// The macro emits an `unsafe impl`. You assert that the disposer releases the
/// owned resources **without freeing the header**, and that `$name` is
/// layout-compatible with `*mut $c_type`.
#[macro_export]
macro_rules! impl_cvalued {
    ($name:ident) => {
        // SAFETY: caller guarantees the inherent `dispose` upholds the
        // contract.
        unsafe impl $crate::traits::CValued for $name {
            #[inline]
            unsafe fn c_dispose(this: ::core::ptr::NonNull<Self>) {
                // SAFETY: caller upholds the `c_dispose` contract.
                unsafe { Self::dispose(this.as_ptr()) }
            }
        }
    };
    ($name:ty, $c_type:ty, $dispose:path) => {
        // SAFETY: caller guarantees that `$dispose` disposes the owned
        // resources of `$c_type` WITHOUT freeing the header, and that
        // `$name` is layout-compatible with `*mut $c_type`.
        unsafe impl $crate::traits::CValued for $name {
            #[inline]
            unsafe fn c_dispose(this: ::core::ptr::NonNull<Self>) {
                // SAFETY: caller upholds the `c_dispose` contract.
                unsafe { $dispose(this.as_ptr() as *mut $c_type) }
            }
        }
    };
}

/// Implement [`CCloned`](crate::CCloned), enabling [`Clone`] and
/// [`try_clone`](crate::CBox::try_clone) on [`CBox<T>`](crate::CBox).
/// Requires an existing [`CDropped`](crate::CDropped) impl, since the clone
/// must be releasable through the same destructor.
///
/// The mechanism is named, never inferred — the two C duplication shapes have
/// different signatures and different results, and which one a type uses is a
/// property of its C API:
///
/// ```ignore
/// // Deep copy: the C routine returns a NEW pointer, NULL on failure.
/// impl_cloned!(EvpPkey, ffi::EVP_PKEY, dup = ffi::EVP_PKEY_dup);
///
/// // Refcount bump: the C routine returns void or a status, and the handle
/// // to keep is the ORIGINAL pointer. Pair with the matching down-ref.
/// impl_dropped!(SslSession, ffi::SSL_SESSION, ffi::SSL_SESSION_free);
/// impl_cloned!(SslSession, ffi::SSL_SESSION, up_ref = ffi::SSL_SESSION_up_ref);
/// ```
///
/// Two-argument forms bind an inherent method instead of a C function:
/// `impl_cloned!(MyCert, dup)` for an
/// `unsafe fn dup(*mut Self) -> *mut Self`, `impl_cloned!(MySession, up_ref)`
/// for a `fn up_ref(&self)`.
///
/// **On a type exposing both, choose `up_ref`.** `Clone` on a refcounted C type
/// means "another handle to the same object" — what the C API and callers
/// expect; a silently deep-copying `Clone` would break identity comparisons and
/// double the allocation cost. Leave the deep copy as an inherent method.
///
/// `$up_ref` is called in statement position and its result discarded, so
/// `void`- and `c_int`-returning up_refs share one arm.
///
/// # Safety
///
/// The macro is safe to invoke but emits an `unsafe impl`. You assert that the
/// named routine leaves the original live and unmodified, and:
///
/// - for `dup =`, that it deep-copies into a fresh, uniquely-owned allocation
///   and returns NULL on failure;
/// - for `up_ref =`, that it increments the reference count of a live object
///   and **cannot fail** — the generated `c_clone` always reports success. A
///   status-returning up_ref fails only on refcount overflow; if that is
///   reachable in your build, hand-write the impl and return `None`.
#[macro_export]
macro_rules! impl_cloned {
    ($name:ident, dup) => {
        // SAFETY: caller guarantees the inherent `dup` upholds the `CCloned`
        // contract — fresh allocation, NULL on failure, source untouched.
        unsafe impl $crate::traits::CCloned for $name {
            #[inline]
            unsafe fn c_clone(
                obj: ::core::ptr::NonNull<Self>,
            ) -> ::core::option::Option<::core::ptr::NonNull<Self>> {
                // SAFETY: caller upholds the `c_clone` contract.
                let dup = unsafe { Self::dup(obj.as_ptr()) };
                ::core::ptr::NonNull::new(dup)
            }
        }
    };
    ($name:ident, up_ref) => {
        // SAFETY: caller guarantees the inherent `up_ref` increments the count
        // of a live object and cannot fail, so the pointer owes one more
        // `c_drop` — the down-ref registered as `CDropped::c_drop`.
        unsafe impl $crate::traits::CCloned for $name {
            #[inline]
            unsafe fn c_clone(
                obj: ::core::ptr::NonNull<Self>,
            ) -> ::core::option::Option<::core::ptr::NonNull<Self>> {
                // SAFETY: caller upholds the `c_clone` contract; `obj` is live,
                // so bumping its count is sound.
                unsafe { obj.as_ref().up_ref() };
                // Same pointer, one more outstanding reference.
                ::core::option::Option::Some(obj)
            }
        }
    };
    ($name:ty, $c_type:ty, dup = $dup:path) => {
        // SAFETY: caller guarantees `$dup` is the C deep-copy for `$c_type`,
        // that `$name` is layout-compatible with `*mut $c_type`, and that the
        // `CDropped` impl releases the duplicate.
        unsafe impl $crate::traits::CCloned for $name {
            #[inline]
            unsafe fn c_clone(
                obj: ::core::ptr::NonNull<Self>,
            ) -> ::core::option::Option<::core::ptr::NonNull<Self>> {
                // SAFETY: caller upholds the `c_clone` contract; `$dup`
                // returns a fresh `*mut $c_type` or NULL.
                let dup = unsafe { $dup(obj.as_ptr() as *mut $c_type) };
                ::core::ptr::NonNull::new(dup as *mut Self)
            }
        }
    };
    ($name:ty, $c_type:ty, up_ref = $up_ref:path) => {
        // SAFETY: caller guarantees `$up_ref` is the C refcount increment for
        // `$c_type`, that `$name` is layout-compatible with `*mut $c_type`, and
        // that the `CDropped` impl registers the matching down-ref.
        unsafe impl $crate::traits::CCloned for $name {
            #[inline]
            unsafe fn c_clone(
                obj: ::core::ptr::NonNull<Self>,
            ) -> ::core::option::Option<::core::ptr::NonNull<Self>> {
                // SAFETY: caller upholds the `c_clone` contract; `obj` is live
                // and layout-compatible with `*mut $c_type`. Statement position
                // discards the result, taking `void`- and `c_int`-returning
                // up_refs alike.
                unsafe { $up_ref(obj.as_ptr() as *mut $c_type) };
                // Same pointer, one more outstanding reference.
                ::core::option::Option::Some(obj)
            }
        }
    };
}
