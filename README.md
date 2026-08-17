# ffibox

Generic smart pointers and traits for building safe Rust wrappers over C
types and pointers. Designed for arbitrary user-space C interop, including the case
where you need direct field access and a C-ABI-compatible layout (porting
C internals to Rust in place), not just opaque-handle wrapping.

## Why

Wrapping a C API in Rust means re-homing C's ownership and lifecycle
conventions into RAII. The recurring shapes:

- Refcounted or exclusive ownership with FFI destructor (`up_ref` / `free`).
- Types with multiple or runtime-conditional destructors.
- By-value object whose teardown disposes fields but not the header.
- Owned length-aware buffer or NUL-terminated `char *` with FFI destructor.
- Type-erased `void *` that stays opaque end to end.

Each shape gets one trait (the per-type lifecycle contract) and one
wrapper (the handle that enforces it). The wrapper picks which teardown
runs; the trait is implemented on the wrapped C type or on a strategy ZST.


## Mental model in one paragraph

No reference to a wrapped C object is ever formed. Each C type gets three Rust
types (see `src/c_type.rs`): the layout newtype `Foo`, `#[repr(transparent)]`
over `CType<ffi::foo>` = `ffi::foo + PhantomPinned`, which embeds by value in a
`#[repr(C)]` mirror and is what an owning pointer points at; and two borrowed
handles `FooRef<'a>` / `FooMut<'a>`, each one pointer wide, carrying the getters
and setters. `&FooRef` covers the handle — Rust-owned stack — so `&self` /
`&mut self` methods are sound on it and `&mut FooRef` reborrows implicitly;
field access projects a raw pointer out of the handle and goes through
`addr_of!` / `addr_of_mut!`. A wrapper carries no lifecycle behaviour by itself
— you bind that by implementing one of the **trait contracts**, and you choose a
**pointer type** that consumes that contract. The split below is the three axes:
*representation* (the three types), *contracts* (what teardown means),
*pointers* (who owns).


## Examples

### CBox

```rust,ignore
use ffibox::{define_ctype, CBox, CDropped};
use core::ptr::NonNull;

mod sys {
    #[repr(C)] pub struct point_st { pub x: i32, pub y: i32 }
    extern "C" {
        pub fn point_new(x: i32, y: i32) -> *mut point_st;
        pub fn point_free(p: *mut point_st);
    }
}

define_ctype!(Point, PointRef, PointMut, sys::point_st);

unsafe impl CDropped for Point {
    unsafe fn c_drop(obj: NonNull<Self>) {
        unsafe { sys::point_free(obj.as_ptr().cast()) }
    }
}

impl Point {
    pub fn new(x: i32, y: i32) -> Option<CBox<Self>> {
        unsafe { CBox::from_raw(sys::point_new(x, y)) }
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

let mut p = Point::new(3, 4).unwrap();
assert_eq!(p.as_ref().x(), 3);
p.as_mut().set_x(5);
// `point_free` runs here.
```

### CVoidBox

```rust,ignore
use ffibox::{CVoidBox, CDropped};
use core::ptr::NonNull;

mod sys {
    extern "C" {
        pub fn arena_alloc(n: usize) -> *mut core::ffi::c_void;
        pub fn arena_fill(p: *mut core::ffi::c_void, n: usize);
        pub fn arena_free(p: *mut core::ffi::c_void);
    }
}

/// Names the destructor class, not the pointee — the bytes stay opaque.
pub struct ArenaFree;

unsafe impl CDropped for ArenaFree {
    unsafe fn c_drop(obj: NonNull<Self>) {
        unsafe { sys::arena_free(obj.as_ptr().cast()) }
    }
}

type ArenaBuf = CVoidBox<ArenaFree>;

let buf: ArenaBuf = unsafe { CVoidBox::from_raw(sys::arena_alloc(64)) }.unwrap();
unsafe { sys::arena_fill(buf.as_ptr(), 64) };
// `arena_free` runs here.
```

### CVal

```rust,ignore
use ffibox::{define_ctype, CVal, CValued};
use core::ptr::NonNull;

mod sys {
    #[repr(C)] pub struct buf_st { pub ptr: *mut u8, pub len: usize }
    extern "C" { pub fn buf_dispose(b: *mut buf_st); }
}

define_ctype!(Buf, BufRef, BufMut, sys::buf_st);

// Rust owns the struct BY VALUE; C owns what its fields point at.
unsafe impl CValued for Buf {
    unsafe fn c_dispose(this: NonNull<Self>) {
        unsafe { sys::buf_dispose(this.as_ptr().cast()) }
    }
}

let b = CVal::new(Buf::zeroed());
assert_eq!(unsafe { (*b.as_ref().as_ptr()).len }, 0);
// `buf_dispose` runs here; the struct itself is dropped inline.
```

### CVec

```rust,ignore
use ffibox::{CVec, CLenDropped};
use core::mem::size_of;

mod sys {
    extern "C" {
        pub fn oids_new(n: usize) -> *mut u32;
        pub fn oids_free(p: *mut u32, n: usize);
    }
}

/// Names the allocator strategy: freeing needs the length back.
pub struct OidsFree;

unsafe impl CLenDropped for OidsFree {
    unsafe fn c_drop_len(ptr: *mut u8, byte_len: usize) {
        unsafe { sys::oids_free(ptr.cast(), byte_len / size_of::<u32>()) }
    }
}

let v: CVec<u32, OidsFree> = unsafe { CVec::from_raw_parts(sys::oids_new(3), 3) }.unwrap();
assert_eq!(v.as_slice().len(), 3);
// `oids_free(ptr, 3)` runs here.
```

---

## Axis 1 — Type representation

| Item | Role | Source |
|------|------|--------|
| `CType<T>` | The layout newtype: `repr(transparent)` over `T` plus `PhantomPinned`. Keeps the C layout (embeds by value in a `#[repr(C)]` mirror, layout-compat with `*mut T`) and pins the address, since C may have recorded it. | `src/c_type.rs` |
| `CPtr<'a, T>` | The storage every borrowed handle is transparent over: one pointer tagged with the borrow's lifetime. `Copy` and covariant in `'a`, like `&'a T`; `Option<CPtr>` is a niche `*const T`. | `src/c_type.rs` |
| `CCell` | The linking trait: `type C` names the wrapped FFI type, `type Ref<'a>` / `type Mut<'a>` name the handles, and the two constructors build them. `define_ctype!` emits it; hand-write it (+ the three structs) for lifetime/generic wrappers. Every wrapper-holding pointer requires it. | `src/c_type.rs` |

| Macro | Generates |
|-------|-----------|
| `define_ctype!(N, NRef, NMut, ffi::c)` | the layout newtype, both handles, and the `impl CCell` linking them |

All three names are spelled out because `macro_rules!` cannot concatenate
identifiers; the order is layout, shared, exclusive. `define_ctype!` does not
support generic parameters for binding lifetimes to the wrapper or expressing
derived sub-types. Hand-write these, implementing the `CCell` contract for
consistency with the crate's conventions.

**Where accessors go.** Getters on `NRef<'a>` taking `&self`, setters on
`NMut<'a>` taking `&mut self`; `NMut` derefs to `NRef`, so getters are written
once. Both project a raw pointer out of the handle:

```rust,ignore
impl SslSessionRef<'_> {
    pub fn timeout(&self) -> u64 {
        unsafe { addr_of!((*self.as_ptr()).timeout).read() }
    }
}
impl SslSessionMut<'_> {
    pub fn set_timeout(&mut self, v: u64) {
        unsafe { addr_of_mut!((*self.as_mut_ptr()).timeout).write(v) }
    }
}
```

**Seam + mutability conventions** (the generated surface, and what hand-written
code follows):

- **`from_raw*` adopts ownership; `from_ptr*` borrows.** `from_raw` /
  `from_raw_parts` (on `CBox` / `CBoxWith` / `CVoidBox` / `CVec` /
  `CrustifyStr`) return an *owning* `Self` whose `Drop` frees; `from_ptr` (on a
  handle, `COut`) returns a *non-owning* handle. Pick the verb by what you
  return.
- **The owning seam speaks the raw C type.** `as_ptr` / `into_raw` / `from_raw`
  on `CBox` take/return `*mut T::C` (the ffi type `ffi::c`, via `CCell`),
  **not** `*mut Wrapper` — so C interop is cast-free
  (`CBox::from_raw(ffi::X_new())`, `ffi::X_free(b.into_raw())`).
- **Owning handles hand out handles, not references.** `as_ref()` / `as_mut()`
  rather than `Deref`: `Deref::Target` cannot name a lifetime taken from
  `&self`, and a handle carries one.
- **Never take `&Wrapper` or `&mut Wrapper`.** Those cover the C object's bytes
  and would assert `noalias` / `readonly` / validity over memory C may write.
  `&NRef` covers one pointer of Rust stack instead. Read and write fields off
  the handle's raw pointer through `addr_of!` / `addr_of_mut!`; never form a
  `&ffi::c` or a reference to a field.

---

## Axis 2 — Lifetime contracts (the traits)

These say **what teardown means**. Bind one (or more) to a wrapper with the
matching `impl_*!` macro (all in `src/macros.rs`; each has a one-arg inherent
form plus the `(N, ffi::c, fn…)` delegating form shown below). The **Bound by**
column is what the macro enables; the pointer types in Axis 3 require these
traits as bounds.

The two base lifecycle traits form a **clone/drop x exclusive/shared grid**.
Each clone trait is a sub-trait of its drop trait — a cloned handle must
be releasable the same way:

|       | exclusive (unique)            | shared (refcounted)                    |
|-------|-------------------------------|----------------------------------------|
| drop  | `CDropped` (`impl_dropped!`) — the `*_free` | `CDropped` (`impl_dropped!`) — the down-ref |
| clone | `CCloned` (`impl_cloned!(N,c,dup = …)`) — the `*_dup` | `CCloned` (`impl_cloned!(N,c,up_ref = …)`) — the `up_ref` |

Same two traits down both columns: the *column* is a property of the C API you
register, never of the Rust type you pick. One macro serves both, with the
mechanism named at the call site (`dup = …` / `up_ref = …`) because the C
signatures differ — a `*_dup` returns a NEW pointer (NULL on failure), an
`up_ref` returns `void`/`c_int` and the handle you keep is the ORIGINAL. The
name is an assertion: nothing is inferred from the return type.

**A type exposing BOTH an `up_ref` and a `*_dup`: the `up_ref` wins.** Bind
`Clone` to the refcount bump (`up_ref = …`) and leave the deep copy as a plain
inherent method (`fn dup(&self) -> Option<CBox<Self>>`). `Clone` on a
refcounted C type means "another handle to the same object" — that is what
callers and the C API both expect; a `Clone` that silently deep-copied would
break identity comparisons and double the allocation cost. Use `dup = …` only
when the type has NO refcount.

| Trait | Contract | Bound by (enables) | Macro | Source |
|-------|----------|--------------------|-------|--------|
| `CDropped` | Destructor on a **fully-constructed** value (`c_drop(NonNull<Self>)`) — the `*_free` for a sole owner, the **down-ref** for a refcounted type. | `CBox`, `CVoidBox`, `CCloned` | `impl_dropped!(N,c,free)` | `src/traits.rs` |
| `CCloned` | Handle duplication (`c_clone(ptr) -> ptr`): a deep-copy `*_dup` returning a **new** pointer, **or** an `up_ref` returning the **same** pointer. Either way the result owes one independent `c_drop`. Opt-in `Clone` for a `CBox` or `CrustifyStr` (a NUL string's `strdup`; length recovered by `strlen`, so pointer-only fits). Supertrait `CDropped`. | `CBox` / `CrustifyStr`: `Clone` | `impl_cloned!(N,c,dup = …)` / `impl_cloned!(N,c,up_ref = …)` | `src/traits.rs` |
| `CValued` | Embedded / by-value teardown for a value that lives **inside** another struct or on the stack (`c_dispose`, no storage of its own to free). | `CVal`, `CValGuard` | `impl_cvalued!(N,c,dispose)` | `src/traits.rs` |
| `CLenDropped` | Release strategy for an `n`-element buffer (`c_drop_len(ptr, byte_len)`), carried by a ZST strategy type you write — the crate ships none. | `CVec` | — (manual impl) | `src/traits.rs` |
| `CElem` | Marker: a plain Rust value, every bit pattern of which is valid — integers, floats, raw pointers, arrays, `MaybeUninit<T>`. A `define_ctype!` wrapper implements `CCell` instead, so `&[Foo]` does not typecheck. | `CVec::as_slice` | — (blanket) | `src/traits.rs` |
| `Owner` | Marker: keeps someone else's C object alive at a stable address, exposing no access to it — the owner half of `CTethered`. | `CTethered<T, O>` | — (manual impl; `CKeepalive` and, with `alloc`, `Arc<O>`) | `src/traits.rs` |
| `CLenCloned` | Length-aware deep-copy strategy for a buffer (`c_clone_len(ptr, byte_len) -> ptr`, a `memdup`) -- the `CCloned` analogue for `CVec` (its `Clone`), needed because the copy carries the byte length `c_clone` cannot. **Byte copy** (POD elements only). Supertrait `CLenDropped`. | `CVec`: `Clone` | — (manual impl) | `src/traits.rs` |

A fully refcounted, cloneable type pairs `impl_dropped!` (the down-ref, so
`CBox` can `Drop`) with `impl_cloned!(…, up_ref = …)` (so it can `Clone`) —
exactly as a sole-owner type pairs `impl_dropped!` with an optional
`impl_cloned!(…, dup = …)`. A drop-only shared handle (a received count you cannot clone)
uses `impl_dropped!` alone; it is simply a `CBox` that is not `Clone`. Teardown is unconditional: a destructor that must be
suppressed on some paths folds that gate **into `c_drop` / `c_dispose`** itself
(or defuse via `into_raw` / `CValGuard::dismiss`).

**The construction phase.** An allocation Rust is still filling in is held as
`CBoxWith<T, D>` with a storage-only `D` — a ZST `CDropper` that reclaims the
raw allocation and touches no field — then promoted with `into_box()` once the
object is formed. One-way, and a type change, so a half-built object cannot
reach code expecting a finished one; bail with `?` before promoting and `D`
reclaims the allocation without running the real destructor.

**Multi-destructor shapes.** A type has one `CDropped`, so a second teardown for
the same C type goes on a `CDropper<T>` policy instead — one `D` per destructor,
and `CBoxWith<T, D>` selects which by its type. Exactly one ever runs:
`CBoxWith`'s `Drop` calls `D::c_drop`, never `T::c_drop`, so a `T` that also
implements `CDropped` keeps that for its `CBox` and the two cannot both fire.
`T: CDropped` is not required at all — a type whose teardowns are all
alternatives needs only `CCell`. A ZST `D` keeps the handle pointer-sized.

**Value-carried teardown — the `*With` strategies.** When teardown is not
recoverable from the pointee, bind a
**policy object** `D` implementing the agent-noun analogue of the base pair:
`CDropper<T>` (drop) and `CCloner<T>` (clone). As with the base pair there is no
separate shared flavour — a refcounted pointee registers the down-ref as
`CDropper::c_drop` and the `up_ref` as `CCloner::c_clone`.

Each method takes `(&self_state, ptr)`, so `D` carries the `fn` / length / struct
into `Drop`. These are **hand-implemented on the state type** (no `impl_*!`
macro) and consumed by `CBoxWith` in Axis 3. Use them when teardown is not
recoverable from `T` alone: runtime state, the construction phase, or a second
destructor. Otherwise register a plain `CDropped` and use `CBox`. All in
`src/traits.rs`.

---

## Axis 3 — Pointer representation (who owns, and in which phase)

These **take ownership** and run the chosen Axis-2 teardown on `Drop`; each
requires its lifecycle trait as a bound (the **Requires** column). Pick by *who
owns* (unique / shared / by value / type-erased) and *which phase of life*
(allocated-but-uninit vs fully constructed).

| Pointer | Owns | Phase | Requires | Source |
|---------|------|-------|----------|--------|
| `CBox<T>` | unique, typed | fully constructed | `T: CDropped + CCell` (`Clone` iff `T: CCloned`) | `src/owned_refs.rs` |
| `CVal<T>` | by value (embedded/stack) | fully constructed | `T: CValued + CCell` | `src/owned_refs.rs` |
| `CValGuard<'a, T>` | borrowed view with teardown, lifetime-bound | fully constructed | `T: CValued + CCell` | `src/owned_refs.rs` |
| `CVec<T, S>` | length-aware buffer | fully constructed | `S: CLenDropped`; `as_slice` iff `T: CElem`, `as_handles` iff `T: CCell` | `src/owned_refs.rs` |
| `CVoidBox<D>` | plain type-erased storage | fully constructed | `D: CDropped` | `src/owned_refs.rs` |
| `CrustifyStr<D>` | owned NUL-terminated C string (`char *`); read-only slice views | fully constructed | `D: CDropped` | `src/owned_refs.rs` |
| `CBoxWith<T, D>` | unique, typed, + inline teardown state | construction and fully constructed | `T: CCell`, `D: CDropper<T>` (`Clone` iff `D: CCloner + Clone`; `into_box` iff `T: CDropped`) | `src/owned_refs.rs` |
| `CKeepalive<T>` | an owner token: teardown only, no access | fully constructed | `T: CDropped + CCell` | `src/owned_refs.rs` |
| `CTethered<T, O>` | a view INTO a parent, holding it alive | fully constructed | `O: Owner` | `src/owned_refs.rs` |

The wrapper-holding pointers all bound **`T: CCell`** (their `T` is a wrapper —
automatic for any `define_ctype!` / hand-written wrapper; it also names the
handles `as_ref()` / `as_mut()` hand out). `CVec` / `CVoidBox` / `CrustifyStr` do **not**: their `S` / `D`
is a *deleter strategy* (or `T` an *element*), not a wrapper.

`CBoxWith` is the **fat** owner: `#[repr(C)]` `{ptr, dropper: D}`, so it is
**not** layout-compatible with `*mut T::C` when `D` carries state (a ZST `D`
stays pointer-sized). Its `from_raw` takes the extra `dropper` argument
(`from_raw(*mut T::C, D)`) — the seam point where the policy is fixed;
`into_raw` hands back `(*mut T::C, D)`. Reach for it when teardown is not
recoverable from `T` alone — runtime state, the construction phase, or a second
destructor, all in Axis 2. Otherwise `CBox`.

### Non-owning pointer wrappers

These represent a raw pointer **without** taking ownership — no teardown, no
lifecycle contract. The borrowed view of a wrapped C object is its
`NRef<'a>` / `NMut<'a>` handle (Axis 1); what remains here is the scalar
out-parameter slot.

| Wrapper | Models | Owns? | Source |
|---------|--------|-------|--------|
| `CSlice<'a, T>` | a borrowed run of `len` contiguous wrapped C objects, yielded as per-element handles by `get` / `iter`. The slice analogue of a `Ref` handle: `&[T]` over wrapped objects would be a reference covering them. Reached with `CVec::as_handles`. | no (shared borrow) | `src/borrowed_refs.rs` |
| `COut<'a, T>` | the write-end of a C `*mut T` **out-parameter** — a `&'a mut MaybeUninit<T>` the callee writes once. `c_out::from_ptr` hides the `*mut T → *mut MaybeUninit<T>` cast at the boundary; `Option<COut>` is layout-compat with `*mut T`. | no (borrowed write-slot) | `src/borrowed_refs.rs` |

---

## Decision procedure

Route by **orthogonal axes** -- not first-match. Each axis narrows a different
dimension: **representation . owned<->borrowed . singleton<->array .
exclusive<->shared . typed<->erased . allocated<->initialized**.

**Axis 1 -- wrapper shape (`define_ctype!` or hand-written).** Everything typed
you wrap is either a `define_ctype!(N, NRef, NMut, ffi::c)` triple (see Axis 1
above) or a hand-written generic mirroring it for (a) a lifetime-carrying
newtype when it holds a field borrowed for a runtime lifetime, and for (b)
derived sub-types when it holds a generic (`void *`) field that could be
monomorphized statically. With the wrapper chosen, route who owns it:

**Axis 2.1 -- owned or borrowed?** Borrowed (non-owning) routes by structural role
and ignores the ownership axes below:
- out-parameter write-end -> `COut<'a, T>`
- self / sibling / parent back-reference -> the type's own `NRef<'a>` handle
- embedded value with a scope-driven teardown -> `CValGuard`
- a type-erased `void*` at the C seam:
  - **genuinely opaque** (an app cookie you never look inside) -> hold a
    `CType<c_void>` field, hand it over with the handle's `as_void_ptr` (the one
    place a bare `CType<T>` is used unwrapped)
  - **erased-but-materializable** (the `void*` is really a `ffi::T`) -> erase a
    `FooRef<'a>` with its `as_void_ptr`, and reconstitute with `from_void_ptr`
- borrowed NUL string (read-only) -> a slice view: `&core::ffi::CStr` / `&str` /
  `&[u8]` (a thin borrowed `const char*` slot is deferred -- no wrapper yet)

Owned -> keep going.

**Axis 2.2 -- singleton, buffer, or string?**
  - A **NUL-terminated C string** (`char *`, terminator-delimited, no stored length) -> `CrustifyStr<D>`
(`D: CDropped`; because `strlen` recovers the length, one deleter covers **both** a
plain free and a length-aware clearing free -- no `CLenDropped` needed). Its bytes
read out as a slice view (`as_c_str` / `as_bytes` / `to_str`); it is **read-only**
(like `core::ffi::CStr`), so the ONLY way to mutate is to drop to the raw `*mut`:
`into_raw()` -> edit -> `from_raw()`.
  - A **counted n-element buffer** (you index / read / write elements) ->
  `CVec<T, S>` (`S`: your own `CLenDropped` strategy). Plain Rust elements
  (`T: CElem`) read out as a real `&[T]` via `as_slice`; wrapped C objects go
  through `as_handles` -> `CSlice<'a, T>`, since a `&[Foo]` would be a reference
  covering them.
  - A **singleton** (one value) -> keep going.

**Axis 2.3 Storage -- by value or own pointer?** Lives **by value inside another
aggregate** -- a struct field, the stack, OR a by-value element of an array /
matrix (no owning pointer of its own) -> `CVal<T>` (`impl_cvalued!`). Has its
**own heap pointer** -> the core matrix below.

**Axis 2.4 Core matrix -- owned heap singleton** (exclusive<->shared X typed<->erased):

|               | **typed `T`** | **type-erased (`void*`)** |
|---------------|---------------|---------------------------|
| **exclusive** (`*_free`) | `CBox<T>` (`impl_dropped!` + optional `impl_cloned!(dup = …)`) | `CVoidBox<D>` (`CDropped`) |
| **shared** (`up_ref` / down-ref) | `CBox<T>` (`impl_dropped!` + `impl_cloned!(up_ref = …)`) | -- rare; raw ptr / `CVoidBox` + manual |

One column of types, two rows of C routines: the typed cell is `CBox<T>` either
way. Adopt an already-built C pointer via `CBox::from_raw`.

**Runtime-state overlay.** If the cell's teardown needs a value chosen at the
wrapping site rather than a fixed `*_free`, swap the thin owner for its fat
sibling: `CBox<T>` -> `CBoxWith<T, D>`, with `D` a strategy carrying the state
(`CDropper`, plus `CCloner` to clone). Orthogonal to the cell. Reach here when
teardown is not recoverable from `T` alone -- runtime state, the construction
phase, or a second destructor, all in Axis 2.

**Allocated<->initialized overlay.** When you **allocate + initialize in Rust**
(porting a ctor), reach the matrix cell through the construction ladder:
- typed (exclusive **or** shared) -> `CBoxWith<T, StorageFree>` with a ZST
  `CDropper` -> `into_box()` -> `CBox<T>`. One ladder for both: the refcount is
  just a field the initializer sets before promoting.
- type-erased -> `CVoidBox` owns its own storage (no separate uninit type).

**Boundary overlay.** An owned pointer that **crosses the FFI boundary** (you
hand C a pointer it will later free, or adopt one C allocated) crosses it on
the owner's raw seam: `into_raw` surrenders it, `from_raw` adopts it, and
the handle's `as_void_ptr` / `from_void_ptr` cross an erased `void *` slot
without transferring ownership. Orthogonal to the cell.



## no_std

Crustify is `#![no_std]` by default. The `std` feature (on by default) selects
`std::process::abort` for the unrecoverable-failure path (`Clone` when the C
copy routine fails); without it that path is a double-panic. One path
allocates: the `alloc` feature adds `Owner for Arc<O>`, so several `CTethered`
children can share a parent C does not refcount. A refcounted parent, or a
single child, stores its `CKeepalive` inline and needs neither.

```toml
[dependencies]
ffibox = { version = "0.1", default-features = false }
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
| Borrowed view              | `FooRef` ZST at the object's address | `&Opaque<T>`      | `FooRef<'a>` / `FooMut<'a>` handles |
| Refcounting                | not modelled                 | `ARef<T>` + `AlwaysRefCounted` | `CBox<T>` + `CDropped`/`CCloned` (down-ref / `up_ref`) |
| Unique owner + destructor  | `Foo`/`FooRef` pair          | `KBox<T>`                 | `CBox<T>` + `CDropped`/`CCloned` (`*_free` / `*_dup`) |
| Direct field access        | no (opaque by design)        | yes                       | yes (`repr(transparent)` over C struct) |
| Buffer cleanup strategy    | no                           | `Allocator` (alloc+free)  | `CVec<T,S>` + `CLenDropped` (free-only) |
| Foreign `void*` slot       | no                           | `ForeignOwnable`          | the handle's `as_void_ptr` / `from_void_ptr` + the owner's `into_raw` / `from_raw` |
| no_std                     | yes                          | n/a (kernel)              | yes                            |

### foreign-types

The incumbent (used by `rust-openssl` since 2014). Models exactly one
shape: a unique owner with a destructor, split into an owned/borrowed
type pair (`Foo` / `FooRef`) where the borrowed tier is an
`Opaque(UnsafeCell<()>)` ZST standing at the object's address. Being
zero-sized, it makes fields unreachable - ideal for wrapping opaque
libraries, unusable for porting C internals where you need to read them; and
a pointer cast out of a zero-size retag carries no provenance for the
object's bytes, which Stacked Borrows rejects. Crustify's borrowed tier
holds the pointer by value instead, so the provenance is the one it was
handed. No refcounting, no conditional cleanup, no buffer wrapper.

### Rust-for-Linux (RFL)

The kernel's `kernel::{types,sync,alloc,list}` modules. Crustify mirrors
RFL's design where it makes sense for user space: `CType<T>` plays the
role of `Opaque<T>` for the layout and `!Unpin`, while the borrow is a
handle rather than a `&Opaque<T>`, so no reference covers the C object. It
diverges on the `void *` seam — where RFL abstracts it behind
`ForeignOwnable`, crustify leaves it on each owner's `into_raw` / `from_raw`
plus the handle's erased borrow pair, since only one owner shape would ever
implement such a trait. It diverges on refcounting too:
where RFL splits `ARef`/`AlwaysRefCounted` from `KBox` and adds `UniqueArc`
for the pre-publication phase, crustify collapses all three into `CBox<T>` —
every handle reaches the object through a raw pointer, so a refcounted share
and a sole owner are the same handle, and the up_ref is just another
`CCloned::c_clone`.
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


## Acknowledgements

This material is based upon work supported by the Defense Advanced Research Projects Agency (DARPA)
Translating All C To Rust (TRACTOR) program under Agreement No. HR00112590134.