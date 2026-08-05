//! # Crustify
//!
//! Generic smart pointers and traits for building safe Rust wrappers over C
//! types. Inspired by the Linux kernel's `ARef<T: AlwaysRefCounted>` and
//! `KBox<T>`, but aimed at arbitrary user-space C interop — including the case
//! where you need direct field access and a C-ABI layout, not just opaque
//! handles.
//!
//! ## Why
//!
//! Wrapping a C API means re-homing C's ownership conventions into RAII. Each
//! recurring shape gets one trait (the lifecycle contract, implemented on the
//! wrapped type or on a strategy object) and one wrapper (the handle that runs
//! it). [`foreign-types`] models the sole-owner case well but not refcounting
//! or strategy-based buffer cleanup; crustify covers all of them uniformly.
//!
//! [`foreign-types`]: https://crates.io/crates/foreign-types
//!
//! ## The owning handles
//!
//! | Type                | Owns                                  | Requires                  |
//! |---------------------|---------------------------------------|---------------------------|
//! | [`CBox<T>`]         | heap object, sole owner **or** refcount share | `T: CDropped + CCell` |
//! | [`CBoxWith<T, D>`]  | the same, plus teardown state `D`; fat unless `D` is a ZST | `D: CDropper<T>` |
//! | [`CBoxUninit<T>`]   | an allocated, not-yet-initialised slot | `T: CDroppedUninit + CCell` |
//! | [`CVoidBox<D>`]     | a type-erased `void *`                 | `D: CDropped`             |
//! | [`CrustifyStr<D>`]  | a NUL-terminated `char *`              | `D: CDropped`             |
//! | [`CVec<T, S>`]      | a length-aware buffer                  | `S: CLenDropped`          |
//! | [`CVal<T>`]         | a value inline, disposing fields only  | `T: CValued + CCell`      |
//! | [`CValGuard<'a, T>`]| the same, borrowed and dismissible     | `T: CValued + CCell`      |
//!
//! Sole ownership and refcounting share **one** type. [`CBox<T>`] is not "the unique
//! pointer" — whether it is exclusive or one share of a refcount depends only
//! on which C routines you register: a `*_free` and a `*_dup` make it
//! exclusive, a down-ref and an `up_ref` make it shared. There is no `CArc`,
//! because it would do nothing differently: no handle hands out `&mut T`
//! (mutation goes through the raw pointer and the [`CCell`] layer), so an
//! aliased and a sole-owner handle have identical capabilities.
//!
//! [`CBox`] is layout-compatible with `*mut T`, and so is `Option<CBox<T>>` via
//! the [`NonNull`](core::ptr::NonNull) niche, so it substitutes for a raw
//! pointer field in a `#[repr(C)]` struct.
//!
//! ## The lifecycle traits
//!
//! Implemented on the wrapped `T`, or on a strategy object for the buffer and
//! `*With` variants. Each clone trait is a sub-trait of its drop trait, since
//! every clone owes the same teardown.
//!
//! | Trait                | Defines                                                |
//! |----------------------|--------------------------------------------------------|
//! | [`CDropped`]         | `c_drop` — a `*_free` **or** the refcount down-ref     |
//! | [`CCloned`]          | `c_clone` — a `*_dup` **or** the `up_ref`              |
//! | [`CDroppedUninit`]   | `c_drop_uninit` — pre-init storage-only free           |
//! | [`CValued`]          | `c_dispose` — dispose fields, leave the header         |
//! | [`CLenDropped`]      | `c_drop_len(ptr, byte_len)` on a buffer strategy       |
//! | [`CLenCloned`]       | `c_clone_len` — the buffer memdup                      |
//! | [`CDropper`] / [`CCloner`] | the same ops for [`CBoxWith`], threading state via `&self` |
//!
//! ## Quick example
//!
//! ```ignore
//! use crustify_prim::{CBox, define_type, impl_cloned, impl_dropped};
//!
//! mod ffi {
//!     #[repr(C)]
//!     pub struct foo_st { /* opaque */ pub _data: [u8; 64] }
//!     extern "C" {
//!         pub fn FOO_new() -> *mut foo_st;
//!         pub fn FOO_up_ref(p: *mut foo_st) -> i32;
//!         pub fn FOO_free(p: *mut foo_st);
//!     }
//! }
//!
//! // `FOO_free` is a down-ref, so registering it as the destructor and
//! // `FOO_up_ref` as the clone makes `CBox<Foo>` a refcounted share.
//! define_type!(Foo, ffi::foo_st);
//! impl_dropped!(Foo, ffi::foo_st, ffi::FOO_free);
//! impl_cloned!(Foo, ffi::foo_st, up_ref = ffi::FOO_up_ref);
//!
//! // The seam speaks the raw C type, so no cast is needed.
//! let a = unsafe { CBox::<Foo>::from_raw(ffi::FOO_new()) }.unwrap();
//! let b = a.clone();           // FOO_up_ref
//! drop(a);                     // FOO_free (refcount > 0, lives on)
//! drop(b);                     // FOO_free (refcount == 0, freed)
//! ```
//!
//! Swap `up_ref = ffi::FOO_up_ref` for `dup = ffi::FOO_dup` and the identical
//! code becomes deep-copy semantics over a sole-owner type.
//!
//! ## `no_std`
//!
//! `#![no_std]` by default. The `std` feature (on by default) selects
//! [`std::process::abort`] for the unrecoverable-failure path; without it that
//! path is a double-panic. Nothing here needs `alloc`.

#![no_std]
#![cfg_attr(docsrs, feature(doc_cfg))]

#[cfg(feature = "std")]
extern crate std;

pub mod borrowed_refs;
pub mod c_type;
pub mod macros;
pub mod owned_refs;
pub mod traits;

/// Back-compatible path for the C out-parameter helper. Generated wrappers call
/// [`c_out::from_ptr`](crate::borrowed_refs::from_ptr); its canonical home is
/// now [`borrowed_refs`].
pub mod c_out {
    pub use crate::borrowed_refs::{from_ptr, COut};
}

// Re-export the primary items at the crate root for convenience.
pub use crate::borrowed_refs::{COut, SelfPtr};
pub use crate::c_type::{CCell, CType};
pub use crate::owned_refs::{
    CBox, CBoxUninit, CBoxWith, CVal, CValGuard, CVec, CVoidBox, CrustifyStr,
};
pub use crate::traits::{
    CCloned, CCloner, CDropped, CDroppedUninit, CDropper, CLenCloned, CLenDropped, CValued,
};
