# Borrows

A borrow is a handle over an object something else releases — C, or a parent
object. It is one pointer wide, carries a lifetime, and is never a Rust
reference to the C object itself.

For the owning side — values whose drop releases the object — see
[ownership.md](ownership.md).

## The handles

| Scope | Type | From | Role |
|-------|------|------|------|
| per C type | `Foo` | `define_ctype!` | the layout, the C struct's size: `#[repr(transparent)]` over `ffi::foo`, embeds by value in a `#[repr(C)]` mirror, and is what a `CBox` points at. Never referenced over C-owned memory. Also the value itself when it owns no resources (a rational `{ num, den }`). |
| | `FooRef<'a>` | `define_ctype!` | shared borrow, one pointer wide, `Copy`; the getters |
| | `FooMut<'a>` | `define_ctype!` | exclusive borrow, move-only; the setters, and the getters through `as_ref()` |
| per C lock | `FooLocked<'a>` | `impl_cguarded!` | exclusive borrow under the C lock, move-only; getters and setters for the state the lock protects, the unlocked getters through `as_ref()` |
| lock | `CGuard<'a, Foo>` | ffibox | a held C lock, unlocking on drop — `MutexGuard`; `as_locked()` → `FooLocked<'_>`, `as_ref()` → `FooRef<'_>` |
| guarded borrow | `CGuardedRef<'a, Foo>` | ffibox | an object under one lock covering all of it (`CGuardedAll`), never released — typically a C global (`'static`); only `lock()`, whose `as_locked()` is `FooMut` — `&Mutex<_>` |
| views | `CSlice<'a, T>` / `CSliceMut<'a, T>` | ffibox | any borrowed run of elements — a buffer's, a struct field's, a getter's — as handles or copies, never a `&[T]` |

A wrapper that needs generic parameters (a lifetime-carrying layout type, or
derived sub-types over a generic field) is written by hand against the same
contract — see [Under the hood](hand-written.md).

## Conventions

- **Accessors live on the handles.** Getters on `FooRef<'a>` taking `&self`,
  setters on `FooMut<'a>` taking `&mut self`; `FooMut` reaches the getters with
  `as_ref()`, so they are written once. Both project a raw pointer out of the
  handle and read or write through `addr_of!` / `addr_of_mut!`.
- **Every holder reaches the handles the same way:** `as_ref()` / `as_mut()` on
  `Foo`, `CBox`, `CVal` and `FooMut` itself (where `as_mut()` is the reborrow
  `&mut` gets implicitly: `helper(m.as_mut())` keeps `m`); `as_ref()` alone on
  `CArc`; `as_ref()` / `as_locked()` on a `CGuard` and on `FooLocked` (where
  `as_locked()` reborrows). `CSliceMut` reborrows the same
  way, and splits and sub-ranges like `&mut [T]`. Never `Deref`:
  `Deref::Target` cannot name a lifetime taken from `&self`, and since `FooRef`
  is `Copy`, a `Deref<Target = FooRef<'a>>` on `FooMut` would let safe code copy
  a shared handle out while keeping the exclusive one.
- **No reference covers C memory.** `&FooRef` covers one pointer of Rust stack,
  never the object. The one place a reference covers a C struct's bytes is
  `Foo::as_ref` / `as_mut` on a value Rust holds inline.
- **The handles' seams are traits the macros implement:** `CRefSeam` on
  `FooRef` (`from_ptr`, `as_ptr` and the `void *` pair), `CMutSeam` on `FooMut`
  (`from_ptr`, `as_mut_ptr` and the `void *` pair) and `CLockedSeam` on
  `FooLocked` (`as_mut_ptr` alone). Import them wherever the seam is used, in
  the crate that invokes `define_ctype!` or in any other that holds its
  handles. Their methods are provided once, by ffibox, so the wrapper crate
  defines no raw-pointer method of its own for them. The inbound half is also
  on `CCell` (`ref_from_ptr`, `mut_from_ptr` and the `void *` twins), so
  generic code over `T: CCell` can borrow a pointer C gives it; those are
  `unsafe`, and the handles' `from_ptr` forward to them. Adopting ownership is
  a different verb on a different type — see
  [ownership.md](ownership.md#conventions).

## Choosing a borrow

Reached when dropping the Rust value does *not* release the object. When it
does, the answer is an owner — see
[ownership.md](ownership.md#choosing-an-owner-what-shape-is-the-owned-thing).

- **No — C, or a parent object, releases it.** Borrow it:
  - one object → `FooRef<'a>` / `FooMut<'a>`, its lifetime tied to whatever
    keeps it alive (a parent's handle, the call);
  - a run of elements → `CSlice<'a, T>` / `CSliceMut<'a, T>`;
  - an object every access reaches under one lock covering all of it —
    typically a C global and the C lock protecting it → `CGuardedRef<'a, Foo>`,
    `'static` for a global, with `impl_cguarded!(Foo, all, …)`;
  - a NUL string → `&CStr` / `&str` / `&[u8]` tied to the owner's borrow;
  - an out-parameter slot → `&'a mut MaybeUninit<T>` from
    `ptr.cast::<MaybeUninit<T>>().as_mut()`; no ffibox type;
  - a `void *` → if it is really a `ffi::foo`, erase a `FooRef` with
    `as_void_ptr` and recover it with `from_void_ptr` (generically,
    `T::ref_from_void_ptr` / `T::mut_from_void_ptr` for `T: CCell`); if it is a
    cookie never looked inside, pass the raw `*mut c_void` through unchanged.

  A borrow that must outlive its parent's scope (stored next to the parent,
  sent to a thread) cannot be expressed as a lifetime; copy the child into an
  owned object instead.
