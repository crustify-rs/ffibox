# ffibox

Generic smart pointers and traits for building safe Rust wrappers over C
types and pointers. Designed for arbitrary user-space C interop, including the
case where you need direct field access and a C-ABI-compatible layout (porting
C internals to Rust in place), not just opaque-handle wrapping.

## Why

Wrapping a C API in Rust means re-homing C's ownership and lifecycle
conventions into RAII. The recurring shapes:

- A C-allocated object with a destructor (`free`), and maybe a copy (`dup`).
- A constructor or allocator (`foo_alloc`, `malloc`, `calloc`) whose result
  must be released by its paired free.
- A refcounted object shared by several holders (`up_ref` / `unref`), maybe
  with a C lock over the state they mutate.
- Types with multiple or runtime-conditional destructors.
- A by-value struct whose teardown disposes its fields but not the struct.
- An owned length-aware buffer or NUL-terminated `char *` with a C destructor.
- A type-erased `void *` that stays opaque end to end.

Each shape gets one owner type, generic over a **policy** — a type you declare
that names the C routines. `impl_cdrop!` and its siblings bind a C routine to a
policy; a type alias names the pair: `pub type FooBox = CBox<Foo, FooFree>;`.

## Mental model in one paragraph

No reference to C-owned memory is ever formed. Each C type gets a layout type
`Foo` (`#[repr(transparent)]` over `ffi::foo`) and two borrowed handles,
`FooRef<'a>` / `FooMut<'a>`, each one pointer wide and carrying the getters and
setters; field access projects a raw pointer out of the handle. Ownership
comes in three shapes: `CBox<Foo, P>` is the sole owner of an object behind a
pointer and releases it through the policy `P`; `CArc<Foo, P>` is one of
several counted references and hands out the shared handle, the exclusive one
only when the count proves it sole, and — for an object with a C lock — a
`CGuard` whose `FooLocked<'a>` handle reaches the state that lock protects; and
`CVal<Foo, P>` holds the object inline and disposes its resources on drop.
A policy that also constructs gives `CBox` and `CArc` safe constructors that
allocate through it, so a safely built owner is always freed by the routine
paired with its allocator.
Strings, buffers and borrowed runs get their own types.
[docs/ownership.md](docs/ownership.md) lists the owners and the policies;
[docs/borrows.md](docs/borrows.md) lists the handles. Each walks from a C
declaration to the type it wants.

## Install

```toml
[dependencies]
ffibox = "0.1"
```

ffibox is `#![no_std]`. The `std` feature (on by default) selects
`std::process::abort` for the unrecoverable-failure path (`Clone` or
`make_mut` when the C copy or up_ref fails); without it that path is a
double-panic. Nothing
allocates.

```toml
[dependencies]
ffibox = { version = "0.1", default-features = false }
```

## Documentation

Every wrapper answers one question first: **does dropping the Rust value
release the object?** This is about who *releases* the object, not who
allocated it — a `CBox` usually points at memory C allocated.

- **Yes** — it is owned → [docs/ownership.md](docs/ownership.md)
- **No** — C, or a parent object, releases it → [docs/borrows.md](docs/borrows.md)

| doc | what is in it |
|-----|---------------|
| [examples.md](docs/examples.md) | seven self-contained wrappers, each compiled as a doctest |
| [ownership.md](docs/ownership.md) | `CBox`, `CVal`, `CStrBox`, `CVec`, `CArc`; the lifecycle policies; picking an owner and a policy |
| [borrows.md](docs/borrows.md) | `Foo`, `FooRef`, `FooMut`, `FooLocked`, `CGuard`, `CGuardedRef`, `CSlice`; where the accessors live; the seam traits |
| [hand-written.md](docs/hand-written.md) | the wrappers `define_ctype!` cannot express |

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

This material is based upon work supported by the Defense Advanced Research Projects Agency (DARPA)
Translating All C To Rust (TRACTOR) program under Agreement No. HR00112590134.
