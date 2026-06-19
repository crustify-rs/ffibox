//! Declarative macros defining safe wrapper types and registering lifecycle
//! traits on them.
//!
//! [`define_type!`](crate::define_type) emits the `#[repr(transparent)]`
//! newtype; the `impl_*!` macros bind a C routine to the matching trait:
//! [`impl_dropped!`](crate::impl_dropped) (a `*_free` or a down-ref),
//! [`impl_cloned!`](crate::impl_cloned) (a `*_dup` or an `up_ref`, named at the
//! call site), [`impl_dropped_uninit!`](crate::impl_dropped_uninit) (the
//! storage-only free), and [`impl_cvalued!`](crate::impl_cvalued) (a by-value
//! dispose).

/// Define a `#[repr(transparent)]` wrapper over a `*-sys` type, using
/// [`CType<T>`](crate::c_type::CType).
///
/// Covers the trivial base case only — a plain `$name(CType<$c_type>)`.
/// Newtypes with lifetime or type parameters (`BufMemBorrowed<'a>`,
/// `Stack<T, S>`) are hand-written against
/// [`CCell`](crate::c_type::CCell); native generics need no macro arm.
///
/// Generates the struct, `unsafe impl CCell for $name { type C = $c_type; }`
/// — which provides the whole seam — and inherent forwarders (`as_ptr` /
/// `from_ptr` / `uninit` / `zeroed`) so the seam is callable without importing
/// `CCell`.
///
/// Every layer of `$name` → `CType<$c_type>` →
/// `UnsafeCell<MaybeUninit<$c_type>>` → `$c_type` is `#[repr(transparent)]`,
/// so `&$name` is ABI-identical to `*const $c_type`, the type embeds by value
/// in a `#[repr(C)]` parent, and `Option<CBox<$name>>` is a niche `*mut
/// $c_type`.
///
/// **No `Deref`**, and no accessor handing out `&$c_type`: such a reference
/// re-introduces the `noalias` / `readonly` guarantees `CType`'s `UnsafeCell`
/// exists to suppress (see [`CType`](crate::c_type), *Scope of `UnsafeCell`
/// protection*). Project fields through raw pointers off `as_ptr()` inside
/// typed accessors instead.
///
/// ## Example
///
/// ```ignore
/// use crustify_prim::define_type;
///
/// mod libssl_sys {
///     #[repr(C)]
///     pub struct ssl_session_st { pub timeout: u64, /* ... */ }
/// }
///
/// define_type! {
///     /// Safe wrapper over the sys-crate `ssl_session_st` struct.
///     SslSession, libssl_sys::ssl_session_st
/// }
///
/// impl SslSession {
///     pub fn timeout(&self) -> u64 {
///         // SAFETY: the struct was initialised by its C constructor before
///         // any Rust read; the field is read through the raw pointer without
///         // ever forming a `&ssl_session_st`.
///         unsafe { (*self.as_ptr()).timeout }
///     }
/// }
/// ```
///
/// Attributes (doc comments, `cfg`, etc.) at the call site are forwarded
/// onto the generated struct.
#[macro_export]
macro_rules! define_type {
    ($(#[$attr:meta])* $name:ident, $c_type:ty) => {
        $(#[$attr])*
        #[repr(transparent)]
        pub struct $name($crate::c_type::CType<$c_type>);

        // SAFETY: the struct above is `#[repr(transparent)]` over
        // `CType<$c_type>` — exactly the invariant `CCell` requires.
        unsafe impl $crate::c_type::CCell for $name {
            type C = $c_type;
        }

        // Forwarders so the seam works without importing `CCell`. The canonical
        // docs and safety reasoning live on the `CCell` methods.
        impl $name {
            /// See [`CCell::as_ptr`](crate::CCell::as_ptr).
            #[inline]
            pub fn as_ptr(&self) -> *mut $c_type {
                self.0.get()
            }

            /// See [`CCell::from_ptr`](crate::CCell::from_ptr).
            ///
            /// # Safety
            ///
            /// `ptr` must point to a valid, initialised `$c_type` (or be null);
            /// the returned reference must not outlive the object.
            #[inline]
            pub unsafe fn from_ptr<'a>(ptr: *mut $c_type) -> Option<&'a Self> {
                // SAFETY: layout-preserving cast per `#[repr(transparent)]`.
                unsafe { (ptr as *const Self).as_ref() }
            }

            /// See [`CCell::uninit`](crate::CCell::uninit).
            pub fn uninit() -> Self {
                Self($crate::c_type::CType::uninit())
            }

            /// See [`CCell::zeroed`](crate::CCell::zeroed).
            #[inline]
            pub fn zeroed() -> Self {
                Self($crate::c_type::CType::zeroed())
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

/// Implement [`CDroppedUninit`](crate::CDroppedUninit), registering the
/// byte-level storage free that [`CBoxUninit`](crate::CBoxUninit) runs to
/// reclaim an allocation whose construction never completed. It must free only
/// the storage, never fields — that is
/// [`CDropped::c_drop`](crate::CDropped::c_drop)'s job on the formed handle.
///
/// The one-arg form uses an inherent `Self::free_uninit`; the three-arg form
/// names the C deallocator, with `::core::ffi::c_void` as the cast type for a
/// `void*`-taking free:
///
/// ```ignore
/// impl_dropped_uninit!(GitOdb, ::core::ffi::c_void, ffi::git__free);
/// ```
///
/// # Safety
///
/// The macro is safe to invoke but emits an `unsafe impl`. You assert that
/// `$free` frees exactly the raw allocation and touches no fields.
#[macro_export]
macro_rules! impl_dropped_uninit {
    ($name:ident) => {
        // SAFETY: caller guarantees `free_uninit` is storage-only, with no
        // field teardown.
        unsafe impl $crate::traits::CDroppedUninit for $name {
            #[inline]
            unsafe fn c_drop_uninit(obj: ::core::ptr::NonNull<Self>) {
                // SAFETY: caller upholds the `c_drop_uninit` contract.
                unsafe { Self::free_uninit(obj.as_ptr()) }
            }
        }
    };
    ($name:ty, $c_type:ty, $free:path) => {
        // SAFETY: caller guarantees `$free` frees the allocation and not its
        // fields, and `$name` is layout-compatible with `*mut $c_type`.
        unsafe impl $crate::traits::CDroppedUninit for $name {
            #[inline]
            unsafe fn c_drop_uninit(obj: ::core::ptr::NonNull<Self>) {
                // SAFETY: caller upholds the `c_drop_uninit` contract.
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
