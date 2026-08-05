# crustify-prim

[![crates.io](https://img.shields.io/crates/v/crustify-prim.svg)](https://crates.io/crates/crustify-prim)
[![docs.rs](https://docs.rs/crustify-prim/badge.svg)](https://docs.rs/crustify-prim)
[![license](https://img.shields.io/crates/l/crustify-prim.svg)](#license)

Generic smart pointers and traits for building safe Rust wrappers over C
types. Designed for arbitrary user-space C interop, including the case
where you need direct field access and a C-ABI-compatible layout (porting
C internals to Rust in place), not just opaque-handle wrapping.

## Why

Wrapping a C API in Rust means re-homing C's ownership and lifecycle
conventions into RAII. The recurring shapes:

- Refcounted shared ownership (`up_ref` / `free`).
- Unique ownership with a destructor (`*_free`, no refcount).
- By-value object whose teardown disposes fields but not the header.
- Length-aware buffer with a cleanup policy (e.g. secure zeroing).
- NUL-terminated `char *` owned by Rust, freed by a C allocator.
- Type-erased `void *` that stays opaque end to end.

Each shape gets one trait (the per-type lifecycle contract) and one
wrapper (the handle that enforces it). The wrapper picks which teardown
runs; the trait is implemented on the wrapped C type or on a strategy ZST.

## Owning smart pointers

| Type                | Pattern                                              | Drop runs           | 64-bit size |
|---------------------|------------------------------------------------------|---------------------|-------------|
| `CBox<T>`           | owned heap object; sole-owner **or** refcounted      | `c_drop`            | 8           |
| `CVoidBox<D>`       | the same, type-erased (`void *`) with deleter class `D` | `D::c_drop`      | 8           |
| `CVal<T>`           | inline by-value storage; disposes fields, not header | `c_dispose`         | size_of `T` |
| `CVec<T, S>`        | length-aware buffer with cleanup strategy `S`        | `S::c_drop_len`     | 16          |
| `CrustifyStr<D>`    | owned NUL-terminated `char *`; read-only views        | `D::c_drop`         | 8           |
| `CBoxWith<T, D>`    | `CBox` with its teardown policy as a value `D`       | `D::c_drop`         | 8 + `D`     |

There is **one** owned-pointer type, not two. Whether a `CBox<T>` is an
exclusive owner or one share of a refcount depends only on which C routines
you register: a `*_free` + `*_dup` make it exclusive, a down-ref + an `up_ref`
make it shared. No wrapper hands out `&mut T` — mutation of an opaque C object
always goes through the raw pointer and `CType`'s interior-mutability cell — so
an aliased handle and a sole-owner handle have identical capabilities and need
no type-level distinction.

`CBox`/`CVoidBox` are `#[repr(transparent)]` over `NonNull<_>` -
layout-compatible with `*mut T`, and their `Option<_>` forms reuse the
`NonNull` niche, so they drop into `#[repr(C)]` pointer fields unchanged.
`CVal<T>` is `#[repr(transparent)]` over `T`. `CVec` stores pointer +
count and is not pointer-compatible. `CBoxWith` is `#[repr(C)]` `{ptr, D}`:
it carries a teardown policy a bare pointer cannot (the
`sk_pop_free(elem_free_fn)` shape), so it is not
pointer-compatible when `D` is non-empty. Reach for it when teardown is not
recoverable from `T` alone: runtime state, or a second policy for one C type
(a ZST `D` keeps the pointer-compatible layout). Otherwise `CBox`.

## Construction handles

The "allocated but not yet initialised" phase. `Drop` runs the
**storage-only** `c_drop_uninit`, never the real destructor (running that over
uninitialised fields is UB), so a construction failure is plain RAII.

| Type                  | `from_raw_uninit` -> ... -> `assume_init` |
|-----------------------|-------------------------------------------|
| `CBoxUninit<T>`       | promotes to `CBox<T>`                     |

## Storage cell and pointer helpers

| Primitive          | Role                                                                                  |
|--------------------|---------------------------------------------------------------------------------------|
| `CType<T>`         | `UnsafeCell<MaybeUninit<T>>`, `!Unpin`: aliasing-safe, uninit-capable cell that every wrapper newtype holds (see `define_type!`). |
| `CCell`            | Trait "I am `#[repr(transparent)]` over `CType<C>`"; implemented with just `type C = ffi::c;`. Provides the whole seam: `as_ptr() -> *mut C`, `as_void_ptr()`, `from_ptr()`, `from_void_ptr()`, `uninit()`, `zeroed()`. |
| `COut<'a, T>`      | Alias for `&'a mut MaybeUninit<T>` - a C scalar out-parameter slot (`*mut T`).         |
| `SelfPtr<'this, T>`| Typed, lifetime-tagged shared pointer for self / parent / sibling borrows; `get() -> &T`, never `&mut`. |

## Lifecycle traits

Implemented on the wrapped `T` (or, for the buffer/strategy traits, on a
strategy ZST or state object). The two core traits form a drop/clone pair,
the clone trait a sub-trait of its drop trait:

| Trait            | For          | Defines                                               |
|------------------|--------------|-------------------------------------------------------|
| `CDropped`       | `CBox`       | `c_drop` — a `*_free`, **or** the refcount down-ref   |
| `CCloned`        | `CBox`       | `c_clone` — a `*_dup`, **or** the `up_ref`; enables `Clone` (sub-trait of `CDropped`) |
| `CValued`        | `CVal`       | `c_dispose` (dispose fields, leave header)            |
| `CDroppedUninit` | `CBoxUninit` | `c_drop_uninit` (pre-init storage-only free)          |
| `CLenDropped`    | `CVec`       | `c_drop_len(ptr, byte_len)` on a strategy ZST         |
| `CLenCloned`     | `CVec`       | `c_clone_len` buffer memdup; enables `Clone` (sub-trait of `CLenDropped`) |
| `CDropper` / `CCloner` | `CBoxWith` | fat-owner strategies — same ops, threading state via `&self` |

There are no built-in strategies: every `CVec` names a `CLenDropped` you
write (see the secure-erase example below).

## Macros

| Macro                  | Emits                                                     |
|------------------------|-----------------------------------------------------------|
| `define_type!`         | wrapper newtype over `CType<C>` + its `CCell` impl        |
| `impl_dropped!`        | `CDropped` from the C `*_free` **or** down-ref            |
| `impl_cloned!`         | `CCloned` from either a C `*_dup` (`dup = …`) or a C `*_up_ref` (`up_ref = …`) |
| `impl_dropped_uninit!` | `CDroppedUninit` from the C storage-only free             |
| `impl_cvalued!`        | `CValued` from the C `*_dispose` / `*_cleanup`            |

## Quick example

```rust,ignore
use crustify_prim::{CBox, define_type, impl_cloned, impl_dropped};

mod ffi {
    #[repr(C)] pub struct foo_st { /* ... */ }
    extern "C" {
        pub fn FOO_new() -> *mut foo_st;
        pub fn FOO_up_ref(p: *mut foo_st) -> i32;
        pub fn FOO_free(p: *mut foo_st);
    }
}

define_type!(Foo, ffi::foo_st);
// `FOO_free` is a down-ref, so this pair makes `CBox<Foo>` a refcounted share.
impl_dropped!(Foo, ffi::foo_st, ffi::FOO_free);                 // Drop  -> c_drop
impl_cloned!(Foo, ffi::foo_st, up_ref = ffi::FOO_up_ref);       // Clone -> c_clone

// The seam speaks the raw C type, so no cast is needed.
let a: CBox<Foo> = unsafe { CBox::<Foo>::from_raw(ffi::FOO_new()) }.unwrap();
let b = a.clone();    // FOO_up_ref
drop(a);              // FOO_free (refcount > 0, lives on)
drop(b);              // FOO_free (refcount == 0, freed)
```

Strategy-based buffer (secure zeroing for key material):

```rust,ignore
use crustify_prim::{CVec, CLenDropped};

struct SecureFree;
unsafe impl CLenDropped for SecureFree {
    unsafe fn c_drop_len(ptr: *mut u8, len: usize) {
        unsafe { ptr.write_bytes(0, len); libc::free(ptr.cast()); }
    }
}
type SecretKey = CVec<u8, SecureFree>;
```

## Conditional teardown

For transaction-guard patterns where a commit/finish step should suppress
the destructor, fold the gate **into `c_drop`** — read the object's own
state and return early:

```rust,ignore
unsafe impl CDropped for Wpacket {
    unsafe fn c_drop(obj: NonNull<Self>) {
        // Skip cleanup once the builder is finalised.
        if unsafe { obj.as_ref() }.is_finalised() { return; }
        unsafe { WPACKET_cleanup(obj.as_ptr() as *mut _) }
    }
}
```

For scope-driven dismissal that isn't a property of the object, defuse via
`CBox::into_raw` (or `CValGuard::dismiss` for the by-value guard).

## no_std

Crustify is `#![no_std]` by default. The `std` feature (on by default) selects
`std::process::abort` for the unrecoverable-failure path (`Clone` when the C
copy routine fails); without it that path is a double-panic. Nothing here
needs `alloc`.

```toml
[dependencies]
crustify-prim = { version = "0.1", default-features = false }
```

## Comparison with alternative systems

Two existing systems solve overlapping problems: `foreign-types` (the
user-space incumbent) and the Linux kernel's Rust-for-Linux (RFL) type
infrastructure. Crustify sits between them - the breadth of RFL's
lifecycle modelling, targeting user-space C, and adding in-place field
access that opaque-handle designs give up.

| Concern                    | `foreign-types`              | Rust-for-Linux            | crustify                       |
|----------------------------|------------------------------|---------------------------|--------------------------------|
| Target                     | opaque user-space C libs     | the Linux kernel          | user-space C, incl. internals  |
| Opaque/aliasing cell       | `Opaque(UnsafeCell<()>)` ZST | `Opaque<T>`               | `CType<T>`                     |
| Refcounting                | not modelled                 | `ARef<T>` + `AlwaysRefCounted` | `CBox<T>` + `CDropped`/`CCloned` (down-ref / `up_ref`) |
| Unique owner + destructor  | `Foo`/`FooRef` pair          | `KBox<T>`                 | `CBox<T>` + `CDropped`/`CCloned` (`*_free` / `*_dup`) |
| Direct field access        | no (opaque by design)        | yes                       | yes (`repr(transparent)` over C struct) |
| Buffer cleanup strategy    | no                           | `Allocator` (alloc+free)  | `CVec<T,S>` + `CLenDropped` (free-only) |
| Foreign `void*` slot       | no                           | `ForeignOwnable`          | `CCell::as_void_ptr` / `from_void_ptr` + the owner's `into_raw` / `from_raw` |
| no_std                     | yes                          | n/a (kernel)              | yes                            |

### foreign-types

The incumbent (used by `rust-openssl` since 2014). Models exactly one
shape: a unique owner with a destructor, split into an owned/borrowed
type pair (`Foo` / `FooRef`) where the borrowed tier is an
`Opaque(UnsafeCell<()>)` ZST. That ZST is deliberately zero-sized, so
fields are unreachable - ideal for wrapping opaque libraries, unusable
for porting C internals where you need to read fields. No refcounting, no
conditional cleanup, no buffer wrapper. Crustify replaces the type-pair
with a single owned handle that derefs to `&T`.

### Rust-for-Linux (RFL)

The kernel's `kernel::{types,sync,alloc,list}` modules. Crustify mirrors
RFL's design where it makes sense for user space: `CType<T>` plays the
role of `Opaque<T>` (`UnsafeCell` + `MaybeUninit` + `!Unpin` folded in). It
diverges on the `void *` seam — where RFL abstracts it behind
`ForeignOwnable`, crustify leaves it on each owner's `into_raw` / `from_raw`
plus `CCell`'s erased borrow pair, since only one owner shape would ever
implement such a trait. It diverges on refcounting too:
where RFL splits `ARef`/`AlwaysRefCounted` from `KBox` and adds `UniqueArc`
for the pre-publication phase, crustify collapses all three into `CBox<T>` —
without `DerefMut` anywhere, a refcounted share and a sole owner are the same
handle, and the up_ref is just another `CCloned::c_clone`.
Crustify deliberately omits the kernel-specific parts (lock framework,
intrusive `List<T, ID>`, `pin_init!`, custom allocators, errno types) -
these are either out of scope for user space or better served by existing
crates.

## Maintainers

- Marius Momeu <marius.momeu@berkeley.edu>

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or
  <https://opensource.org/licenses/MIT>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms
or conditions.
