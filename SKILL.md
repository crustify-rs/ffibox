---
name: crustify-prim
roles: [translator]
description: >-
  Choose and apply the right crustify smart pointer / trait when representing a
  C type's ownership and lifetime in safe Rust. Use when wrapping a raw C
  pointer or by-value C struct, or when porting a C allocator, constructor, or
  destructor — i.e. when deciding among CBox / CVal / CVec / CBoxUninit /
  CVoidBox / CrustifyStr / CValGuard / CBoxWith / SelfPtr / COut and the
  traits that drive them (CDropped / CCloned / CDroppedUninit / CValued /
  CLenDropped, plus the fat-owner strategies CDropper / CCloner).
---

# Representing C types and pointers in safe Rust with `crustify`

This skill is a **router**, not a reference. It maps a C type's observable
lifecycle shape onto the correct primitive, then points you at the primitive's
own doc comment for the details. The source doc comments are the single source
of truth — read them; do not assume from the name.

Crate name in code is `crustify_prim` (`use crustify_prim::{CBox, CVal, ...}`).
Paths below are relative to the crate root; anchor on the **symbol**, not a line
number.

## Mental model in one paragraph

Every wrapper is `#[repr(transparent)]` over a `CType<T>` (see `CType` /
`CCell` in `src/c_type.rs`): `UnsafeCell<MaybeUninit<T>> + PhantomPinned`.
`repr(transparent)` keeps it layout-compatible with `*mut T` (drop-in for raw
pointer fields); `UnsafeCell` strips `noalias`/read-only so C may mutate behind
shared refs; `MaybeUninit` means a wrapper does **not** assume its bytes are
initialized. A wrapper carries no lifecycle behaviour by itself — you bind that
by implementing one of the **trait contracts** , and
you choose a **pointer type** that consumes that contract. The split below is
the three axes: *representation* (one shape), *contracts* (what teardown means),
*pointers* (who owns, and in which phase of life).

---

## Axis 1 — Type representation

| Item | Role | Source |
|------|------|--------|
| `CType<T>` | The universal cell: `repr(transparent)` (layout-compat with `*mut T`), `UnsafeCell` (unbinds `noalias`/read-only so C can mutate through `&self`), `MaybeUninit` (never assumes init). | `src/c_type.rs` |
| `CCell` | The wrapper trait. Implement with just `type C = ffi::c;` — the whole seam (`as_ptr` / `as_void_ptr` / `from_ptr` / `uninit` / `zeroed`) is **provided**. `define_type!` emits it; hand-write it (+ a `#[repr(transparent)]` struct) for lifetime/generic wrappers. Every wrapper-holding pointer requires it. | `src/c_type.rs` |

| Macro | Generates |
|-------|-----------|
| `define_type!(N, ffi::c)` | the base wrapper + `impl CCell` contract |

`define_type!` does not support generic parameters for binding lifetimes to the
wrapper or expressing derived sub-types. Hand-write these manually, implementing
the `CCell` contract for consistency with the crate's conventions. 

**Seam + mutability conventions** (the generated surface, and what hand-written
code follows):

- **`from_raw*` adopts ownership; `from_ptr*` borrows.** `from_raw` /
  `from_raw_parts` (on `CBox` / `CVoidBox` / `CVec` / `CrustifyStr`) return an
  *owning* `Self` whose `Drop` frees; `from_ptr` (on a `define_type!` wrapper,
  `COut`, `SelfPtr`) returns a *non-owning* reference / handle. Pick the verb by
  what you return.
- **The owning seam speaks the raw C type.** `as_ptr` / `into_raw` / `from_raw`
  on `CBox` take/return `*mut T::C` (the ffi type `ffi::c`,
  via `CCell`), **not** `*mut Wrapper` — so C interop is cast-free
  (`CBox::from_raw(ffi::X_new())`, `ffi::X_free(b.into_raw())`). `as_void_ptr`
  gives the type-erased `*mut c_void` for generic `void*` shims (`memcpy`, …).
- **Shared borrows only -- never `&mut Self`.** `&mut` asserts a `noalias` the
  FFI seam cannot honour (C keeps its own aliasing pointer -> UB); `CType`'s
  `UnsafeCell` rescues *shared* aliasing, not `&mut`. So `CCell` / `define_type!`
  offer no `from_ptr_mut`. **Mutate through `&self`** via `as_ptr()` + `addr_of_mut!` raw
  writes -- the `UnsafeCell` makes that sound. Read fields the same way off
  `as_ptr()`; never form a `&c_type` / `&field`.

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
| `CDroppedUninit` | Storage-only cleanup for the **pre-construction** phase (free the raw allocation; do *not* run field destructors). | `CBoxUninit` | `impl_dropped_uninit!(N,c,free)` | `src/traits.rs` |
| `CValued` | Embedded / by-value teardown for a value that lives **inside** another struct or on the stack (`c_dispose`, no storage of its own to free). | `CVal`, `CValGuard` | `impl_cvalued!(N,c,dispose)` | `src/traits.rs` |
| `CLenDropped` | Release strategy for an `n`-element buffer (`c_drop_len(ptr, byte_len)`), carried by a ZST strategy type you write — the crate ships none. | `CVec` | — (manual impl) | `src/traits.rs` |
| `CLenCloned` | Length-aware deep-copy strategy for a buffer (`c_clone_len(ptr, byte_len) -> ptr`, a `memdup`) -- the `CCloned` analogue for `CVec` (its `Clone`), needed because the copy carries the byte length `c_clone` cannot. **Byte copy** (POD elements only). Supertrait `CLenDropped`. | `CVec`: `Clone` | — (manual impl) | `src/traits.rs` |

A fully refcounted, cloneable type pairs `impl_dropped!` (the down-ref, so
`CBox` can `Drop`) with `impl_cloned!(…, up_ref = …)` (so it can `Clone`) —
exactly as a sole-owner type pairs `impl_dropped!` with an optional
`impl_cloned!(…, dup = …)`. A drop-only shared handle (a received count you cannot clone)
uses `impl_dropped!` alone; it is simply a `CBox` that is not `Clone`. Teardown is unconditional: a destructor that must be
suppressed on some paths folds that gate **into `c_drop` / `c_dispose`** itself
(or defuse via `into_raw` / `CValGuard::dismiss`).

Key contrast: **`CDropped` vs `CDroppedUninit`.** `CDropped` runs the full
destructor on a formed object; `CDroppedUninit` only releases the raw storage of
a half-built one. A self-contained ctor port uses both — `CDroppedUninit` while
initializing, `CDropped` once `assume_init`'d.

**Value-carried teardown — the `*With` strategies.** When teardown is not
recoverable from the pointee, bind a
**policy object** `D` implementing the agent-noun analogue of the base pair:
`CDropper<T>` (drop) and `CCloner<T>` (clone). As with the base pair there is no
separate shared flavour — a refcounted pointee registers the down-ref as
`CDropper::c_drop` and the `up_ref` as `CCloner::c_clone`.

Each method takes `(&self_state, ptr)`, so `D` carries the `fn` / length / struct
into `Drop`. These are **hand-implemented on the state type** (no `impl_*!`
macro) and consumed by `CBoxWith` in Axis 3. Use them when teardown is not
recoverable from `T` alone: runtime state, or a second policy for one C type.
Otherwise register a plain `CDropped` and use `CBox`. All in `src/traits.rs`.

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
| `CVec<T, S>` | length-aware buffer | fully constructed | `S: CLenDropped` | `src/owned_refs.rs` |
| `CVoidBox<D>` | plain **type-erased** storage | fully constructed | `D: CDropped` | `src/owned_refs.rs` |
| `CrustifyStr<D>` | owned **NUL-terminated C string** (`char *`); read-only slice views | fully constructed | `D: CDropped` | `src/owned_refs.rs` |
| `CBoxUninit<T>` | unique | **allocated, not yet initialized** | `T: CDroppedUninit + CCell` | `src/owned_refs.rs` |
| `CBoxWith<T, D>` | unique, typed, **+ inline teardown state** | fully constructed | `T: CCell`, `D: CDropper<T>` (`Clone` iff `D: CCloner + Clone`) | `src/owned_refs.rs` |

The wrapper-holding pointers all bound **`T: CCell`** (their `T` is a wrapper —
automatic for any `define_type!` / hand-written wrapper; it also unlocks the
`*mut T::C` seam above). `CVec` / `CVoidBox` / `CrustifyStr` do **not**: their `S` / `D`
is a *deleter strategy* (or `T` an *element*), not a wrapper.

`CBoxWith` is the **fat** owner: `#[repr(C)]` `{ptr, dropper: D}`, so it is
**not** layout-compatible with `*mut T::C` when `D` carries state (a ZST `D`
stays pointer-sized). Its `from_raw` takes the extra `dropper` argument
(`from_raw(*mut T::C, D)`) — the seam point where the policy is fixed;
`into_raw` hands back `(*mut T::C, D)`. Reach for it when teardown is not
recoverable from `T` alone: runtime state, or a second policy for one C type
(a ZST `D` keeps the pointer-compatible layout). Otherwise `CBox`.

### Non-owning pointer wrappers

These represent a raw pointer **without** taking ownership — no teardown, no
lifecycle contract. They sit alongside the owning pointers but answer a
different question (how the pointer is *used*, not who frees it).

| Wrapper | Models | Owns? | Source |
|---------|--------|-------|--------|
| `COut<'a, T>` | the write-end of a C `*mut T` **out-parameter** — a `&'a mut MaybeUninit<T>` the callee writes once. `c_out::from_ptr` hides the `*mut T → *mut MaybeUninit<T>` cast at the boundary; `Option<COut>` is layout-compat with `*mut T`. | no (borrowed write-slot) | `src/borrowed_refs.rs` |
| `SelfPtr<'this, T>` | a typed, `'this`-tagged **shared** (`&T`-shaped) borrow for self-referential / sibling / parent pointers; hands out `&T` / `*const T`, never `&mut`. `Copy`; `Option<SelfPtr>` is layout-compat with `*const T`. | no (shared borrow) | `src/borrowed_refs.rs` |

---

## Decision procedure

Route by **orthogonal axes** -- not first-match. Each axis narrows a different
dimension: **representation . owned<->borrowed . singleton<->array .
exclusive<->shared . typed<->erased . allocated<->initialized**.

**Axis 1 -- wrapper shape (`define_type!` or hand-written).** Everything typed you wrap is
either a `define_type!` newtype (see Axis 1 above): the base `define_type!(N, ffi::c)`,
or a hand-written generic mirroring it for (a) lifetime-carrying newtype when it holds a field
borrowed for a runtime lifetime, and for (b) derived sub-types when it holds a generic (`void *`)
field that could be monomorphized statically. With the wrapper chosen, route who owns it:

**Axis 2.1 -- owned or borrowed?** Borrowed (non-owning) routes by structural role
and ignores the ownership axes below:
- out-parameter write-end -> `COut<'a, T>`
- self / sibling / parent back-reference -> `SelfPtr<'this, T>`
- embedded value with a scope-driven teardown -> `CValGuard`
- a type-erased `void*` at the C seam:
  - **genuinely opaque** (an app cookie you never look inside) -> hold a
    `CType<c_void>` field, hand it over with `CCell::as_void_ptr` (the one place
    a bare `CType<T>` is used unwrapped)
  - **erased-but-materializable** (the `void*` is really a `ffi::T`) -> erase a
    `&Foo` with `as_void_ptr`
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
  `CVec<T, S>` (`S`: your own `CLenDropped` strategy);
  its elements are themselves by-value (`CVal`) or owned pointers (a nested
  `CBox` per element).
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
teardown is not recoverable from `T` alone: runtime state, or a second policy
for one C type.

**Allocated<->initialized overlay.** When you **allocate + initialize in Rust**
(porting a ctor), reach the matrix cell through the uninit ladder:
- typed (exclusive **or** shared) -> `CBoxUninit<T>` (`impl_dropped_uninit!`)
  -> `assume_init()` -> `CBox<T>`. One ladder for both: the refcount is just a
  field the initializer sets before `assume_init`.
- type-erased -> `CVoidBox` owns its own storage (no separate uninit type).

**Boundary overlay.** An owned pointer that **crosses the FFI boundary** (you
hand C a pointer it will later free, or adopt one C allocated) crosses it on
the owner's raw seam: `into_raw` surrenders it, `from_raw` adopts it, and
`CCell::as_void_ptr` / `CCell::from_void_ptr` cross an erased `void *` slot
without transferring ownership. Orthogonal to the cell.