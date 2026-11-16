# ffibox

ffibox provides generic smart pointers and traits for building safe Rust
wrappers over C types and pointers. It works in both directions:

- memory that C owns and Rust holds; and
- memory that Rust owns and C holds, calls or reads.

ffibox is designed for arbitrary user-space C interop, not only for wrapping
opaque handles. It also covers the case where you need direct field access and
a C-ABI-compatible layout, as when you port C internals to Rust in place.

## Example

Wrap a C object that has a constructor, a copy routine and a destructor:

```rust,no_run
use ffibox::{define_ctype, impl_cdrop, impl_cdupclone, CBox, CMutSeam, CRefSeam};

mod sys {
    #[repr(C)] pub struct point_st { pub x: i32, pub y: i32 }
    unsafe extern "C" {
        pub fn point_new(x: i32, y: i32) -> *mut point_st;
        pub fn point_dup(p: *mut point_st) -> *mut point_st;
        pub fn point_free(p: *mut point_st);
    }
}

// The layout type and its two borrowed handles.
define_ctype!(Point, PointRef, PointMut, sys::point_st);

// The policy: a type you declare, bound to the C routines.
#[derive(Clone, Copy, Debug, Default)]
pub struct PointFree;
impl_cdrop!(PointFree, Point, sys::point_free);
impl_cdupclone!(PointFree, Point, sys::point_dup);

pub type PointBox = CBox<Point, PointFree>;

impl Point {
    pub fn new(x: i32, y: i32) -> Option<PointBox> {
        unsafe { PointBox::from_c(sys::point_new(x, y)) }
    }
}

// Getters live on the shared handle, setters on the exclusive one.
impl PointRef<'_> {
    pub fn x(&self) -> i32 {
        unsafe { core::ptr::addr_of!((*self.as_ptr()).x).read() }
    }
}
impl PointMut<'_> {
    pub fn set_x(&mut self, v: i32) {
        unsafe { core::ptr::addr_of_mut!((*self.as_mut_ptr()).x).write(v) }
    }
}

// Your users write no `unsafe` and see no raw pointer.
let mut p = Point::new(3, 4).unwrap();
assert_eq!(p.as_ref().x(), 3);
p.as_mut().set_x(5);
let q = p.clone();           // point_dup
// `point_free` runs twice here.
```

[docs/examples.md](docs/examples.md) has eight self-contained wrappers, each
compiled as a doctest.

## Why

When you wrap a C API in Rust, you express C's ownership and lifecycle
conventions as RAII. The same shapes recur:

- A C-allocated object with a destructor (`free`), and maybe a copy (`dup`).
- A constructor or allocator (`foo_alloc`, `malloc`, `calloc`) whose result
  must be released by its paired free.
- A refcounted object shared by several holders (`up_ref` / `unref`), maybe
  with a C lock over the state they mutate.
- A type with multiple or runtime-conditional destructors.
- A by-value struct whose teardown disposes its fields but not the struct.
- An owned length-aware buffer or NUL-terminated `char *` with a C destructor.
- A type-erased `void *` that stays opaque end to end.

Each shape gets one owner type, generic over a **policy**: a type you declare
that names the C routines. `impl_cdrop!` and its siblings bind a C routine to
a policy, and a type alias names the pair:
`pub type FooBox = CBox<Foo, FooFree>;`.

## Mental model

One rule holds throughout: no reference to C-owned memory is formed while
anything may write or free that memory. You reach objects through handles.
Only a run of bytes that nothing writes while it is lent becomes a `&CStr` or
a `&[u8]`.

### Layout types and handles

Each C type gets three Rust types:

| Type | What it is |
|---|---|
| `Foo` | The layout type: `#[repr(transparent)]` over `ffi::foo`. It is `Foo<'r>` when the C object borrows its parent. |
| `FooRef<'a>` | The shared borrowed handle, one pointer wide. It carries the getters. |
| `FooMut<'a>` | The exclusive borrowed handle, one pointer wide. It carries the setters, and reaches the getters through `as_ref()`. |

Field access projects a raw pointer out of the handle.

### Owners

Ownership comes in three shapes:

| Owner | What it holds |
|---|---|
| `CBox<Foo, P>` | The sole owner of an object behind a pointer. It releases the object through the policy `P`. |
| `CArc<Foo, P>` | One of several counted references. It hands out the shared handle, and the exclusive handle only when the count proves the reference sole. For an object with a C lock, it hands out a `CGuard`, whose `FooLocked<'a>` handle reaches the state that the lock protects. |
| `CVal<Foo, P>` | The object inline. It disposes the object's resources on drop. |

A policy that also constructs gives `CBox` and `CArc` safe constructors that
allocate through it. An owner that you build safely is therefore always freed
by the routine paired with its allocator.

Strings, buffers and borrowed runs get their own types. A buffer drops its
elements, so a `foo **` of owned objects releases each one.

### The other direction

Memory that Rust owns and C holds, calls or reads uses the same terms:

| Primitive | What it does |
|---|---|
| `RustAlloc` | The policy over Rust's allocator, so that an object Rust allocates is held as one that C allocated. |
| `CAllocator` | A C allocation family as the standard `Allocator`, so a `Box` or a `Vec` holds memory that C frees. |
| `CClosure`, `CPayload` | Put a Rust closure behind a C function pointer and its context: `CClosure` for one call, `CPayload` until C releases it. |
| `CFn`, `CFnBox` | Hold a C function and its context that Rust calls. |
| `CCallback`, `CCallbackBox` | Hold either half: a Rust closure or a C function. |
| `CTable` | Holds Rust state behind a C table of function pointers that C calls into. `impl_cvtable!` mints its slots from a trait that the Rust side implements. |
| `export_*!` macros | Offer a policy to C as the routine it calls, with the owner and handle types in its signature. |

[docs/ownership.md](docs/ownership.md) lists the owners and the policies, and
[docs/borrows.md](docs/borrows.md) lists the handles. Each walks from a C
declaration to the type it wants.

## Install

```toml
[dependencies]
ffibox = "0.1"
```

ffibox needs Rust 1.100 or later (`rust-version` in `Cargo.toml`), the first
release with the standard `Allocator` interface, which `CAllocator` implements
over a C allocation family.

### Features

ffibox is `#![no_std]`. It has two features:

- **`std`**, on by default:
  - selects `std::process::abort` for the unrecoverable-failure path, which
    is `Clone` or `make_mut` when the C copy or up_ref fails. Without `std`,
    that path is a double-panic; and
  - lets `CClosure`, `CTable`'s slots and `callback::barrier` catch a panic
    before it reaches C.
- **`alloc`**, which `std` enables:
  - adds `RustAlloc`, `CPayload`, `CCallbackBox` and `CTable`, which allocate
    through Rust's global allocator; and
  - adds the conversions between ffibox's owners and `Box` / `Vec`.

Nothing else allocates, except through a policy that you chose, as
`CAllocator` allocates through its family.

To build without the default features, and so without `std` and `alloc`:

```toml
[dependencies]
ffibox = { version = "0.1", default-features = false }
```

## Documentation

For every wrapper, answer one question first: **does dropping the Rust value
release the object?** The question is about who *releases* the object, not
who allocated it: a `CBox` usually points at memory that C allocated.

- **Yes**: the object is owned. Read [docs/ownership.md](docs/ownership.md).
- **No**: C, or a parent object, releases it. Read
  [docs/borrows.md](docs/borrows.md).

When it is Rust's memory that C holds, calls or reads, read
[docs/outbound.md](docs/outbound.md).

| Document | Contents |
|---|---|
| [examples.md](docs/examples.md) | eight self-contained wrappers, each compiled as a doctest |
| [ownership.md](docs/ownership.md) | `CBox`, `CBoxState`, `CVal`, `CStrBox`, `CVec`, `CArc`; the lifecycle policies; picking an owner and a policy; `Box` and `Vec` over a C allocator (`CAllocator`) |
| [borrows.md](docs/borrows.md) | `Foo`, `FooRef`, `FooMut`, `FooLocked`, `CGuard`, `CGuardedRef`, `CSlice`, `CSliceMut`, `CNullTerminated`; where the accessors live; the seam traits |
| [outbound.md](docs/outbound.md) | Rust's side: `RustAlloc`, the callbacks (`CClosure`, `CPayload`, `CFn`, `CFnBox`, `CCallback`, `CCallbackBox`), C tables (`CTable`), out-parameters, the panic barrier, the `export_*!` macros |
| [vtables.md](docs/vtables.md) | C tables of function pointers implemented in Rust: `CTable`, `impl_cvtable!`, their receivers, re-entry, panics and release, and the design |
| [patterns.md](docs/patterns.md) | C's iteration, error and callback conventions as Rust's iterators, `Result` and closures |
| [hand-written.md](docs/hand-written.md) | the wrappers `define_ctype!` cannot express (a layout type generic over a type), and the owners no policy expresses |
| [goals.md](docs/goals.md) | what the primitives aim for, in both directions, and how each aim is checked |

If you are upgrading, [CHANGELOG.md](CHANGELOG.md) says what each release
changes, and how to migrate code that still compiles but behaves differently.

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

## Acknowledgements

This material is based upon work supported by the Defense Advanced Research
Projects Agency (DARPA) Translating All C To Rust (TRACTOR) program under
Agreement No. HR00112590134.
