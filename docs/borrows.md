# Borrows

A borrow is a handle over an object that something else releases: C, or a
parent object. It is one pointer wide and carries a lifetime. It is never a
Rust reference to the C object itself.

For the owning side, the values whose drop releases the object, see
[ownership.md](ownership.md).

## The handles

| Scope | Type | From | Role |
|-------|------|------|------|
| per C type | `Foo` | `define_ctype!` | The layout type, the size of the C struct. It is `#[repr(transparent)]` over `ffi::foo`, embeds by value in a `#[repr(C)]` mirror, and is what a `CBox` points at. It is never referenced over C-owned memory. It is also the value itself when it owns no resources (a rational `{ num, den }`). |
| | `FooRef<'a>` | `define_ctype!` | The shared borrow, one pointer wide and `Copy`. It carries the getters. |
| | `FooMut<'a>` | `define_ctype!` | The exclusive borrow, move-only. It carries the setters, and reaches the getters through `as_ref()`. |
| per C lock | `FooLocked<'a>` | `impl_cguarded!` | The exclusive borrow under the C lock, move-only. It carries the getters and setters for the state that the lock protects, and reaches the unlocked getters through `as_ref()`. |
| lock | `CGuard<'a, Foo>` | ffibox | A held C lock, which unlocks on drop. Its Rust counterpart is `MutexGuard`. `as_locked()` gives `FooLocked<'_>`, and `as_ref()` gives `FooRef<'_>`. |
| guarded borrow | `CGuardedRef<'a, Foo>` | ffibox | A borrow of an object under one lock that covers all of it (`CGuardedAll`). Nothing in Rust owns or releases the object; it is typically a C global (`'static`). The borrow has only `lock()`, and the `as_locked()` of the guard it returns is `FooMut`. Its Rust counterpart is `&Mutex<_>`. |
| views | `CSlice<'a, T>` / `CSliceMut<'a, T>` | ffibox | Any borrowed run of elements: a buffer's, a struct field's, a getter's. It gives the elements as handles or copies, never as a `&[T]`. Only a run of plain values that nothing writes while it is lent is a `&[T]` instead, as [Choosing a borrow](#choosing-a-borrow) describes. |
| | `CNullTerminated<'a, T>` | ffibox | A NULL-terminated array of pointers (`char **`, `foo **`), iterated as `&CStr`s or `FooRef`s. |

`define_ctype!` also declares a layout type that carries a lifetime, for a C
object that keeps a pointer to its parent:
`define_ctype!(Blob<'r>, BlobRef, BlobMut, ffi::blob, phantom = &'r Repo)`
gives `Blob<'r>`, `BlobRef<'a, 'r>` and `BlobMut<'a, 'r>`. The `phantom`
marker decides how `Blob` varies in `'r`.

A wrapper that needs type parameters (derived sub-types over a generic field)
is written by hand against the same contract; see
[hand-written.md](hand-written.md). Its policies still come from the macros
where the macros take the type.

## Conventions

- **Accessors live on the handles.** Write the getters on `FooRef<'a>`,
  taking `&self`, and the setters on `FooMut<'a>`, taking `&mut self`.
  `FooMut` reaches the getters with `as_ref()`, so you write them once. Both
  kinds project a raw pointer out of the handle and read or write through
  `addr_of!` / `addr_of_mut!`.
- **C bitfields are read by field.** A mirror holds the storage unit as a
  `CBitfield<W>` (or `CAtomicBitfield<W>`, when threads share it), and the
  getters and setters pass the field's `CBits<W>`, declared once in C's
  order with `CBits::first` / `next`. No mask is written by hand.
- **Every holder reaches the handles the same way:**
  - `as_ref()` / `as_mut()` on `Foo`, `CBox`, `CBoxState`, `CVal` and
    `FooMut` itself. On `FooMut`, `as_mut()` is the reborrow that a `&mut`
    gets implicitly: `helper(m.as_mut())` keeps `m`.
  - `as_ref()` on `CArc`. Its exclusive handle comes only from `get_mut()` /
    `make_mut()`.
  - `as_ref()` / `as_locked()` on a `CGuard` and on `FooLocked`. On
    `FooLocked`, `as_locked()` reborrows.

  `CSliceMut` reborrows the same way, and splits and takes sub-ranges like
  `&mut [T]`.

  The handles are never reached through `Deref`, for two reasons.
  `Deref::Target` cannot name a lifetime taken from `&self`. And `FooRef` is
  `Copy`, so a `Deref<Target = FooRef<'a>>` on `FooMut` would let safe code
  copy a shared handle out while it keeps the exclusive one.
- **No reference covers C memory anything may write.** A `&FooRef` covers one
  pointer of Rust stack, never the object. The one place where a reference
  covers a C struct's bytes is `Foo::as_ref` / `as_mut` on a value that Rust
  holds inline. A run of bytes that an object lends, a NUL string or a
  counted run, is lent as `&CStr` or `&[u8]` only while nothing writes it, as
  [Choosing a borrow](#choosing-a-borrow) describes. Otherwise it is a
  `CSlice`.
- **The handles' seams are traits the macros implement:**
  - `CRefSeam` on `FooRef`: `from_ptr`, `as_ptr` and the `void *` pair;
  - `CMutSeam` on `FooMut`: `from_ptr`, `as_mut_ptr` and the `void *` pair;
    and
  - `CLockedSeam` on `FooLocked`: `as_mut_ptr` alone.

  Import them wherever you use the seam, whether in the crate that invokes
  `define_ctype!` or in any other crate that holds its handles. ffibox
  provides their methods once, so the wrapper crate defines no raw-pointer
  method of its own for them.

  The inbound half of the seam is also on `CCell`: `ref_from_ptr`,
  `mut_from_ptr` and the `void *` twins. Generic code over `T: CCell` can
  therefore borrow a pointer that C gives it. Those methods are `unsafe`, and
  the handles' `from_ptr` forward to them. Their caller vouches that the
  pointee is a valid `Foo`, not only an initialised C object. A valid `Foo`
  includes the borrows that its lifetimes name.

  Adopting ownership is a different verb on a different type; see
  [ownership.md](ownership.md#conventions).

## Choosing a borrow

You reach this section when dropping the Rust value does *not* release the
object. When it does, the answer is an owner; see
[ownership.md](ownership.md#choosing-an-owner-what-shape-is-the-owned-thing).

- **No — C, or a parent object, releases it.** Borrow it:
  - one object → `FooRef<'a>` / `FooMut<'a>`. Tie its lifetime to whatever
    keeps the object alive (a parent's handle, the call);
  - a run of elements → `CSlice<'a, T>` / `CSliceMut<'a, T>`;
  - a counted run of immutable bytes that an object lends,
    `const uint8_t *data` with its `size_t len` (from a getter pair, or two
    fields) → `&'a [u8]` from `core::slice::from_raw_parts`, tied to the
    shared handle's `'a`, as a NUL string is a `&CStr`. A run of other
    `CPlainElem` values is the same `&'a [T]`. The SAFETY comment shows, from
    C's contract, that:
    - nothing writes, moves or frees the bytes while the shared handle lives.
      No routine reachable through a `FooRef` writes, grows or reallocates
      them: not one on `FooRef`, not one on a `FooLocked` taken beside it,
      and, if `Foo` is `Sync`, not one on another thread. A write that is
      reachable only through `FooMut` is excluded by the borrow;
    - they live as long as the object does. They are freed with the object,
      or with a parent that the lifetime covers, and not by the next call
      that refills a shared buffer; and
    - `data` addresses `len` initialised bytes in one allocation. For a NULL
      or empty run, return `&[]` without calling `from_raw_parts`, since
      `from_raw_parts` takes no NULL even for a length of 0.

    Where any of these fails, the run is a `CSlice<'a, u8>` built with
    `CSlice::from_raw_parts`, which forms no reference: `elems`,
    `copy_to_slice` and `to_cvec` copy the bytes out. The conditions fail,
    for example, for a cache, a buffer that the library refills or
    reallocates behind a `const` pointer, and state that another thread
    writes. The element type does not decide between `&[u8]` and `CSlice`,
    since a `u8` is plain either way. Who may write the
    bytes decides. A caller that keeps the bytes past the object's borrow
    takes an owned copy;
  - a NULL-terminated array of pointers → `CNullTerminated<'a, T>`, an
    `Iterator` of the elements' borrows. A `count` + `get(i)` pair is
    `(0..n).map(get)` over the safe getter, and a `->next` chain is
    `iter::successors` over the safe getter;
  - an object that every access reaches under one lock covering all of it,
    typically a C global and the C lock that protects it →
    `CGuardedRef<'a, Foo>`, `'static` for a global, with
    `impl_cguarded!(Foo, all, …)`;
  - a NUL string → `&CStr` / `&str` / `&[u8]` tied to the owner's borrow,
    on the terms of a counted byte run above;
  - the object a pointer field holds, filled once and kept while its
    struct lives (C's weak pointer, `repo->_odb`) → `&'a O` from
    `CAtomicOwner<O>::get`, where `O` is the field's owner;
  - an out-parameter slot → `&'a mut MaybeUninit<T>` from
    `ptr.cast::<MaybeUninit<T>>().as_mut()`. It needs no ffibox type;
  - a `void *` → one of two cases:
    - if it is really a `ffi::foo`, erase a `FooRef` with `as_void_ptr` and
      recover it with `from_void_ptr`. Generic code uses
      `T::ref_from_void_ptr` / `T::mut_from_void_ptr` for `T: CCell`;
    - if it is a cookie that is never looked inside, pass the raw
      `*mut c_void` through unchanged.

  A borrow that must outlive its parent's scope (stored next to the parent,
  sent to a thread) cannot be expressed as a lifetime. Copy the child into an
  owned object instead.
