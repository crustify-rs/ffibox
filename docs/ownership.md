# Ownership

An owner is a Rust value whose drop releases a C object. The shape of the
thing owned decides which owner you reach for. *What* the owner's drop does is
a separate choice, the [policy](#policies--what-teardown-means). The owner's
type carries both.

For the borrowed side — handles over objects C or a parent releases — see
[borrows.md](borrows.md).

## The owners

| Scope | Type | From | Role |
|-------|------|------|------|
| owners | `CBox<Foo, P>` | ffibox | the sole owner of an object behind a pointer, released by `P` on drop; `Clone` when `P` deep-copies; built safely by `new` / `new_zeroed` / `new_uninit` when `P` constructs |
| | `CBoxState<Foo, P>` | ffibox | `CBox` under a policy with state (an allocator context, an element-free callback), stored beside the pointer; crosses to C as a raw pointer |
| | `CVal<Foo, P>` | ffibox | an owned value held inline, its resources disposed by `P` on drop |
| | `CStrBox<P>` | ffibox | an owned NUL-terminated `char *`; read-only `CStr` / `str` / byte views |
| | `CVec<T, P>` | ffibox | an owned `(ptr, len)` array, NULL when empty; drops its elements, then frees the buffer; `&[T]` for plain elements and owners, `CSlice` for wrapped C objects; `from_fn` / `new_uninit` build one in `P`'s allocator, `from_slice` copies a Rust slice into it |
| shared owner | `CArc<Foo, P>` | ffibox | one counted reference to a refcounted object; `Clone` through the up_ref; the shared handle always, `get_mut` / `make_mut` when the count proves it sole or after a copy, and `lock()` → `CGuard` when `Foo` is `CGuarded` over part of it |
| standard owners over a C allocator | `Box<T, CAllocator<P>>`, `Vec<T, CAllocator<P>>` | std, over ffibox's `CAllocator` | a value or buffer Rust owns natively in storage C allocates or frees with the family `P` (`malloc` / `realloc` / `free`); to and from `CBox` / `CVec` over the same `CAllocator<P>` without copying — see [`Box` and `Vec` over a C allocator](#box-and-vec-over-a-c-allocator) |

## Conventions

- **Adopting a pointer is `unsafe`; giving one out is not — as on `Box`.**
  `from_raw*` / `from_c*` are `unsafe`, since they assert ownership of what the
  pointer addresses. `into_raw*` / `into_c` and `as_ptr` / `as_c_ptr` are safe,
  since a raw pointer asserts nothing until someone dereferences it. The
  owners' `from_raw` / `into_raw` / `as_ptr` take and return a pointer to the
  Rust-side pointee. `from_c` / `into_c` / `as_c_ptr` on `CBox`, `CBoxState`
  and `CArc` take and return a pointer to the C type, so C interop needs no
  cast. The handles have seams of their own — see
  [borrows.md](borrows.md#conventions).
- **The owners behave like their std counterparts where ownership allows.**
  `CStrBox` derefs to `CStr` and compares and hashes by content, as `CString`
  does, and it converts into a `CString`. A `CVec` of plain data or owners
  derefs to a slice and compares element-wise, as `Vec` does. A `CVec` of C
  objects does not deref: you reach its elements through handles.
- **Constructing through the policy is safe.** `new`, `new_zeroed` and
  `new_uninit` (each with a `_with(policy)` form) allocate through the routine
  the owner frees with, and return `None` when C fails, as `from_raw` does on
  NULL. The one `unsafe` step of building through a policy is `assume_init`,
  which promotes storage C has filled through `as_c_ptr`. There is no safe
  `write(value)`: a C object is initialised where it lives.
- **`from_raw*` adopts ownership; `from_ptr*` borrows.** Pick the verb by what
  you return.
- **Thread safety is opt-in for wrapped C types, opt-out for raw memory.**
  `Foo` carries a zero-sized marker that withholds `Send` / `Sync`. `CBox`,
  `CVal`, `CVec` and the views over a `Foo` inherit `Send` / `Sync` once you
  write `unsafe impl Send / Sync for Foo` with a safety proof. They inherit
  them on the terms of their std counterparts:
  - `CBox` like `Box`;
  - `CArc` with `Foo: Send + Sync`, like `Arc`;
  - `CGuardedRef` with `Foo: Send`, like `&Mutex<_>`.

  Owners that never name a `Foo` follow their policy alone. These are
  `CStrBox` and a `CVec` of plain elements (`CVec<u8, _>`). A unit-struct
  policy is `Send + Sync`, so **they cross threads by default**. That is right
  for an allocator that any thread may free into (`free`, a library's
  `lib_free`). For an allocator that must free on the allocating thread, opt
  out by giving the policy a `PhantomData<*const ()>` field.

## Choosing an owner: what shape is the owned thing?

You reach this section once you know that dropping the Rust value is what
releases the object. When it is not, the answer is a borrow instead — see
[borrows.md](borrows.md#choosing-a-borrow).

- **A NUL-terminated string** (`char *`, no stored length) → `CStrBox<P>`.
  `strlen` recovers the length, so one `CDrop<c_char>` policy covers a plain
  free and a clearing free alike. Its views are read-only, like `CStr`.
- **A counted array** → `CVec<T, P>`, one policy per allocator family. Plain
  elements (`CPlainElem`: integers, floats, raw pointers, `MaybeUninit`) and
  owners (`CSliceElem`) read out as a real `&[T]` because the buffer is
  exclusively owned. Wrapped C objects come out as handles through
  `as_handles` → `CSlice`, since `&[Foo]` would cover them. Where C returns
  NULL with 0 for an empty array, adopt with `from_raw_parts_or_empty`. Plain
  `from_raw_parts` treats every NULL as failure.

  Dropping a `CVec` drops its elements, as a `Vec` does, so the element type
  says what each slot owns. A wrapper around a `CVec` therefore never drops
  the elements itself: a `Drop` that does frees each one twice. Up to
  ffibox 0.1.1 a `CVec` freed the buffer alone. Migrate code written for that
  behaviour as the [CHANGELOG](../CHANGELOG.md) describes.

  | C array | element type |
  |---------|--------------|
  | `foo **`, each entry owned, NULL allowed | `Option<CBox<Foo, FooFree>>` |
  | `char **`, each string owned | `Option<CStrBox<P>>` |
  | `foo *`, each inline value disposed | `CVal<Foo, FooUninit>` |
  | freed with its elements by one routine (`foo_array_free(arr, n)`) | an owner from the rows above, under the array allocator's policy, not that routine |
  | entries borrowed from elsewhere | the bare pointer |

  The element type releases the elements, and the policy releases the array
  alone: `CLenDrop::c_drop_len` frees the buffer after the `CVec` has dropped
  every element. So a routine that frees both is never the `CLenDrop`, for
  three reasons:

  - Over owner elements, the routine frees each element a second time.
  - Bare pointers are `CPlainElem`, so over them the `CVec` derefs to
    `&mut [*mut foo]`. Through that slice, safe code stores any pointer, or
    NULL, in a slot that the routine then frees.
  - The policy is a public type, so it also frees the buffer of a `CVec` of
    any other element type. The routine would read that buffer's bytes as
    `foo *`s.

  Where the routine's body is only those frees, the `CVec` releases the same
  way on drop, and the routine is not called. libgit2's `git_strlist_free` is
  such a routine (`git__free` on each string, then on the array): a
  `CVec<Option<CStrBox<GitFree>>, GitFree>` releases as it does. Where the
  routine does more, the wrapper is hand-written — see
  [hand-written.md](hand-written.md#an-array-freed-by-one-routine).

  Adopting a buffer of owners (`from_raw_parts` and its siblings) asserts
  that each non-null element is an allocation of its own: a pointer repeated
  in two slots is freed twice.
- **One object held inline** — a local, a field, an array element, with no
  pointer of its own:
  - owns no resources → the `Foo` itself (`Foo::zeroed()` when it is
    `CZeroable`, or a wrapper constructor that has C fill a local);
  - owns resources and stands alone → `CVal<Foo, P>`;
  - embedded in a parent C struct → a bare `Foo`, which the parent's
    teardown disposes (a `Drop` on it would dispose twice);
  - address-sensitive (points into itself, or C recorded its address) → not
    inline at all: behind a pointer, as below.
- **One object behind a pointer** → `CBox<Foo, P>` if this is the only
  reference, `CArc<Foo, P>` if it is one of several counted ones. `CBox` and
  `CArc` own only `CCell` layout types, which come from `define_ctype!` or
  are written by hand. A `void *` payload is therefore bytes (`CVec<u8, P>`)
  or a typed owner of its own.

## Policies — what teardown means

Every owner's destructor is a **policy**: a type you declare that implements
the lifecycle traits. It is usually a unit struct deriving
`Clone, Copy, Debug, Default`. Bind a C routine to it with the `impl_*!`
macros, or implement the traits by hand when teardown needs runtime state. A
unit struct is `Send + Sync`, so a `CStrBox` or plain-element `CVec` under it
frees on whichever thread drops it. The policy of a thread-bound allocator
carries `PhantomData<*const ()>` instead (see [Thread safety](#conventions)
above).

| Trait | Defines | Macro | Drives |
|-------|---------|-------|--------|
| `CDrop<T>` | `c_drop` — a `*_free`, or a refcount down-ref | `impl_cdrop!(P, Foo, f)`; `_str` for `c_char` | `CBox`, `CStrBox`, `CArc` |
| `CDupClone<T>: CDrop<T>` | `c_dup` — a deep copy (a NEW pointer, NULL on failure) | `impl_cdupclone!(P, Foo, f)`; `_str` for `strdup` | `Clone` / `try_clone` |
| `CRefClone<T>: CDrop<T>` | `c_up_ref` — a refcount increment on the SAME pointer; `c_is_sole_owner` (default `false`) | `impl_crefclone!(P, Foo, f)`; `…, ok = |r| r == 1`, `…, sole = g` | `CArc`'s `Clone`, `get_mut`, `make_mut` |
| `CLenDrop` | `c_drop_len` — a buffer free, given the byte length; the buffer only, never its elements | `impl_clendrop!(P, f)` | `CVec` |
| `CLenClone: CLenDrop` | `c_clone_len` — a buffer memdup (`T: Copy` only) from any readable source; `ALIGN` (default 1) bounds the element types it can copy | `impl_clenclone!(P, f)`, `impl_clenclone!(P, f, align = 16)` | `CVec`'s `Clone`, `CVec::from_slice`, `CSlice::to_cvec` |
| `CLenAlloc: CLenDrop` | `c_alloc_len` — uninitialised buffer storage (`malloc(n)`); `ALIGN` as above; `c_realloc_len` — `realloc`, provided as `malloc`, copy and `free` | `impl_clenalloc!(P, f)`, `…, align = 16`, `…, realloc = g` | `CVec::from_fn`, `CVec::new_uninit`, `CAllocator<P>` |
| `CDispose<T>` | `c_dispose` — `*_uninit` / `*_clear` on a value | `impl_cdispose!(P, Foo, f)` | `CVal` |
| `CNew<T>: CDrop<T>` | `c_new` — a zero-argument `*_alloc` / `*_new` | by hand | `CBox::new`, `CArc::new` |
| `CAlloc<T>: CDrop<T> + CDrop<MaybeUninit<T>>` | `c_alloc` — uninitialised storage (`malloc`); the free must not read it | by hand | `CBox::new_uninit` |
| `CAllocZeroed<T>: CDrop<T>` | `c_alloc_zeroed` — zero-filled storage (`calloc`) | by hand | `CBox::new_zeroed`, `CArc::new_zeroed` |
| `CZeroable`, on `Foo` | all-zero is valid for every accessor, safe wrapper and paired teardown; provides `Foo::zeroed()` | `unsafe impl CZeroable for Foo {}` | `new_zeroed`, `Foo::zeroed()` |
| `CGuarded`, on `Foo` | `c_lock` / `c_unlock`; the `Locked` handle; the `Scope` it covers | `impl_cguarded!(Foo, FooLocked, lock = f, unlock = g)`; `…, ok = |r| r == 1`; `impl_cguarded!(Foo, all, …)` for `CGuardedAll` | `CArc::lock`, `CGuardedRef::lock` |

`Foo` in the Macro column is any `CCell` layout type, whether `define_ctype!`
declares it or it is written by hand. A layout type that carries a lifetime is
written `Foo<'_>`, as **A layout type with a lifetime** below describes. Two
macros are the exception: `impl_cdupclone!` and `impl_cguarded!` name the
lifetime in a leading list, as `<'r> P, Foo<'r>, f` and
`<'r> Foo<'r>, FooLocked, lock = …`.

**Because the policy is a type parameter, an owner cannot exist without a
destructor, and one C type can have several.** `CBox<Foo, FooFree>` can sit
next to `CBox<Foo, FooUnref>`: exactly one destructor ever runs per box, and
the box's type chooses it. Each clone trait sits on the same policy as the
drop trait, so a `*_dup` is always settled by its `*_free`.

**`CBox` is a sole owner; `CArc` shares.** `CBox::as_mut` hands out the
exclusive handle, which is sound only while nothing else uses the object, so
`CBox` clones only by deep copy. A refcounted object fits a `CBox` while the
box holds its only reference, with the down-ref as `c_drop`. `CArc::from`
shares the object, and `CBox::try_from` (or `try_into_box`) takes it back
once the count is 1 again.

**Shared and mutable means locked — usually, part of it.** A `CArc` has no
unlocked write path. Its exclusive handle needs one of two things:

- The count proves the reference sole. This is `get_mut`, which needs
  `c_is_sole_owner`: an Acquire read of a count that covers every reference,
  C's own included.
- A private copy. This is `make_mut`, which copies through `CDupClone`. It
  never makes a byte copy, which would duplicate the object's
  sub-allocations, its count and its lock.

A C object rarely locks all of itself. It locks the state its holders mutate,
such as a cache or a list of backends, and leaves the rest fixed once set up.
That is `Arc<T>` with a `Mutex` field, not `Arc<Mutex<T>>`, and the handles
split the same way:

| State | Handle | Reached through |
|-------|--------|-----------------|
| fixed once shared; routines that lock internally | `FooRef` | `CArc::as_ref()`, always |
| protected by the lock — reads too; routines that need it held | `FooLocked` | `CArc::lock()` → `CGuard::as_locked()` |
| anything | `FooMut` | `CArc::get_mut()`, sole reference only |

A getter for protected state never goes on `FooRef`: it would race with a
writer holding the lock through another clone. `CGuarded`'s contract makes C
take the same lock, or require its caller to hold it. The contract also makes
the layout type invariant in any lifetime the protected state can hold, as a
`Mutex` field does, so a covariant `CArc` cannot shrink that lifetime.

The lock must not be recursive. A second `lock` on the same thread blocks
forever, or panics when the lock reports the self-deadlock (`EDEADLK`),
rather than hand out a second `FooLocked`. A failing C lock call panics, as
`std`'s locks do. There is no poisoning.

An object whose lock covers all of it is `CGuardedAll`
(`impl_cguarded!(Foo, all, …)`). Its locked handle is `FooMut`, so you reach
it only through a `CGuardedRef`, which has no unlocked path. `CArc::lock`
rejects it at compile time.

**Routines are plain paths, type-checked.** Each macro calls the routine with a
pointer of the exact C type, so a routine for the wrong type does not compile.
Where a call can fail, as an up_ref or a lock can, the routine either returns
`()` or is bound with `ok = |r| r == 1`, where the closure tests for whatever
the routine's success value is. A status code is never silently discarded, so
`pthread_mutex_lock`'s `EDEADLK` becomes a panic rather than a second locked
handle.

A routine of any other shape goes behind a small `unsafe fn` adapter, which
you pass to the macro by path. Such shapes are:

- a destructor that takes the slot (`ffi::foo_free(&mut p)`);
- an up_ref or a copy that answers through an out-parameter
  (`ffi::foo_dup(&mut out, p)`);
- a `void *` allocator free;
- a routine with extra arguments.

The adapter is private and has a `# Safety` section of its own. It sits beside
the policy it serves, where the crate keeps its unsafe code (the module that
holds it, if the crate gathers its unsafe code in one), as a hand-written impl
would. Prefer the adapter to a
hand-written `unsafe impl CDrop` (or `CRefClone`, `CDupClone`), for two
reasons. The macro keeps the impl's shape and its type check of the C
pointer. And the adapter adds no public unsafe surface: it adds only a
private `unsafe fn` and the unsafe block that calls the routine, which a
hand-written impl holds too.

Teardown is unconditional. Where a gate suppresses teardown on some paths,
fold the gate into the routine itself.

**A layout type with a lifetime.** A layout type can carry a lifetime, as
`Blob<'r>` does over an object that keeps its repository. Whether
`define_ctype!` declares it or it is written by hand, it takes its policies
from the macros. `impl_cdrop!`, `impl_crefclone!` and `impl_cdispose!` accept
it with the lifetime spelled `'_`. The impl header turns `'_` into a
parameter, so one policy serves `Blob<'r>` for every `'r`. A named lifetime
(`Blob<'r>`) is undeclared there and does not compile.

```rust,ignore
impl_cdrop!(BlobFree, Blob<'_>, ffi::blob_free);
// `blob_dup` answers through an out-parameter: an adapter gives it the shape.
impl_crefclone!(BlobFree, Blob<'_>, blob_up_ref, ok = |r| r == 0);
pub type BlobArc<'r> = CArc<Blob<'r>, BlobFree>;
```

Two macros take the lifetime named instead, in a list ahead of their
arguments:

- `impl_cdupclone!`, because its `c_dup` returns the type, and a `'_` there
  elides to `&self`'s lifetime rather than the parameter's;
- `impl_cguarded!`, because it names the type in a struct and an associated
  type, where `'_` is not allowed.

```rust,ignore
impl_cdupclone!(<'r> BlobFree, Blob<'r>, ffi::blob_copy);
impl_cguarded!(<'r> Cache<'r>, CacheLocked, lock = ffi::cache_lock, unlock = ffi::cache_unlock);
// CacheLocked<'a, 'r>, the handle a held lock grants
```

A layout type that is generic over a type, rather than a lifetime, has no
elided form: all its policies are hand-written.
[examples.md](examples.md#a-layout-type-with-a-lifetime--define_ctype-with-phantom)
shows the whole wrapper.

**Runtime-state policies.** When teardown needs a value chosen at the wrapping
site (`list_pop_free(list, elem_free_fn)`, `pool_free(pool, p)`), write the
policy by hand. The policy is a struct that holds the state and implements
`CDrop<T>` (and `CDupClone<T>` + `Clone` to clone). Hold the object in a
`CBoxState`, which stores the policy beside the pointer. Adopt with
`from_raw_with(ptr, ElemFree(f))` and release with `into_raw_with()`. `CBox`,
`CArc` and `CStrBox` hold a stateless policy, so that each is the pointer it
stands in for. A policy with state fails to build in them. A routine known at
compile time needs no state: a unit-struct policy per routine names it.

**Construction goes through the policy too.** Bind a constructor to the policy
and the owner's type pairs allocation with teardown, just as it pairs `*_dup`
with `*_free`. A safely built `CBox<Foo, P>` was allocated by `P`, and `from_raw`
is the only way around that, which is why it is `unsafe`.

- `CNew` is a type's own zero-argument constructor (`frame_alloc`).
  Constructors that take arguments stay inherent functions on `Foo` returning a
  `CBox<Foo, P>`, because a trait signature cannot carry their parameters.
- `CAllocZeroed` asks for zero-filled storage and needs `Foo: CZeroable`. That
  marker means all-zero is a state every safe accessor, every safe wrapper and
  the paired teardown accept, not merely a valid Rust value. Leave it off a
  type whose C constructor sets non-zero defaults.
- `CAlloc` gives uninitialised storage, which C fills through `as_c_ptr` and
  the caller promotes with `unsafe assume_init`. Because a box dropped before
  that point frees through `CDrop<MaybeUninit<Foo>>`, the policy's free must not
  read the object, so a destructor like `frame_free` cannot implement it.

**The construction phase.** Hold an allocation that is still being filled in
under a storage-only policy, either `new_uninit` or a hand-written
`CBox<Foo, StorageFree>`. Promote it with `unsafe with_policy(FooFree)` once
it satisfies the full destructor. `with_policy` is an `unsafe fn`: its caller
vouches that the object satisfies every invariant `FooFree`'s `c_drop` relies
on. It returns a pair: the promoted box and the old policy.
If you bail with `?` before promoting, only the storage is freed.

## Choosing a policy

For boxes, strings and buffers.

| C routines | Policy |
|------------|--------|
| `foo_free` | `impl_cdrop!` |
| `foo_alloc()` + `foo_free` | `impl_cdrop!` + a hand-written `CNew` → `CBox::new()` |
| `malloc` / `realloc` + `free`, an allocation family | `impl_clendrop!` + `impl_clenalloc!` on one policy `P`; `CAllocator<P>` is then the storage-only policy of every owner (`CBox`, `CStrBox`, `CVec`) and the `Allocator` of `Box` / `Vec` |
| `malloc` / `calloc` + `free`, for any type, without a family | a generic hand-written `CDrop<T>` + `CAlloc<T>` / `CAllocZeroed<T>` |
| `foo_free` + `foo_dup` | `impl_cdrop!` + `impl_cdupclone!` |
| `foo_unref`, held as the only reference | `impl_cdrop!` with the down-ref; no `Clone` |
| `foo_unref` + `foo_up_ref`, shared, read-only | `impl_cdrop!` + `impl_crefclone!` → `CArc` |
| … and a readable count | `impl_crefclone!(…, sole = g)`: `get_mut` without a copy |
| … with state every holder mutates under a C lock | + `impl_cguarded!(Foo, FooLocked, …)` → `CArc::lock`; protected getters and setters on `FooLocked` |
| a global, never freed, under one C lock | no policy: `impl_cguarded!(Foo, all, …)` → `CGuardedRef<'static, Foo>` |
| a counted reference with no up_ref | `impl_cdrop!` alone → `CArc` without `Clone` |
| teardown needs runtime state | a hand-written policy; adopt with `from_raw_with` |
| built in place, not by a C constructor | `CAlloc` → `new_uninit` → `unsafe assume_init`, then `unsafe with_policy` if a fuller destructor takes over |
| a routine of another shape: a slot, an out-parameter, extra arguments | the macro, over a private `unsafe fn` adapter passed by path, not a hand-written impl |
| any of the above, over a layout type with a lifetime (`Blob<'r>`) | the same macros with `Blob<'_>`; `impl_cdupclone!` and `impl_cguarded!` name the lifetime in a leading list instead: `<'r> P, Blob<'r>, f` and `<'r> Blob<'r>, BlobLocked, lock = …` (or `<'r> Blob<'r>, all, lock = …`) |
| Rust allocates it (`alloc` feature) | `RustAlloc`: objects, buffers and strings from Rust's global allocator |

An owned pointer crosses the FFI boundary on the owner's raw seam:

- `into_raw` surrenders it to C;
- `from_raw` adopts one that C allocated;
- `from_c_out` / `from_raw_out` adopt one that a C routine returns through an
  out-parameter.

`RustAlloc` is the policy over Rust's global allocator. An object or buffer
that Rust allocates is held by the same owners and handles as one that C
allocated. When a type's allocation moves from C to Rust during a port, only
its owner's alias changes: `CBox<Foo, FooFree>` becomes
`CBox<Foo, RustAlloc>`. What `RustAlloc` allocates, only `RustAlloc` frees.
[outbound.md](outbound.md) covers offering it to C.

## `Box` and `Vec` over a C allocator

A C allocation family, `malloc` / `realloc` / `free` and the alignment they
vouch for, is a `CLenAlloc` policy, declared once:

```rust,ignore
#[derive(Clone, Copy, Debug, Default)]
pub struct LibAlloc;
impl_clendrop!(LibAlloc, lib_buf_free); // adapter over `lib_free(void *)`
impl_clenalloc!(LibAlloc, ffi::lib_malloc, align = 16, realloc = ffi::lib_realloc);

pub type LibAllocator = CAllocator<LibAlloc>;
pub type LibBox<T> = Box<T, LibAllocator>;
pub type LibVec<T> = Vec<T, LibAllocator>;
pub type FooBox = CBox<Foo, LibAllocator>; // storage-only: `lib_free` releases it
```

A policy that already implements `CLenAlloc` by hand needs nothing more.
`CAllocator<P>` over it is two things:

- the standard `core::alloc::Allocator`, which is what `System` and `Global`
  are for Rust's heap;
- a storage-only policy for `CBox`, `CStrBox` and `CVec`, as `RustAlloc` is
  over Rust's heap.

`CAllocator<P>` is an `Allocator` only over a `Copy` policy, which has no drop
to unwind. A unit struct is `Copy`.

**Which owner.** The standard owners and ffibox's owners both hold memory of
the family. What decides between them is what C does with the value beyond
its storage.

| the value | owner |
|-----------|-------|
| a Rust value (a native-layout struct, a run of plain data or owners) whose storage C allocates or frees, and nothing more: C keeps no pointer into it while Rust holds it | `Box<T, CAllocator<P>>`, `Vec<T, CAllocator<P>>` |
| a buffer Rust grows (`push`, `extend`, `try_reserve`) before handing it to C, or one C allocated that Rust grows | `Vec<T, CAllocator<P>>`, into a `CVec` to hand over |
| a C object reached through handles (a `CCell` layout type), or one C keeps a pointer to, or one address-sensitive | `CBox<Foo, P>` |
| released by a C lifecycle routine, not only `free` (`foo_free` disposing fields, a down-ref) | `CBox<Foo, FooFree>`, `CArc`: no `Allocator` releases it |
| a counted object, a string | `CArc`, `CStrBox`: std's `Arc::new_in` and `String` with an allocator are not stable yet |
| pinned, its address recorded by C | `CBox`: `Pin<Box<T, A>>` needs `StaticAllocator`, not stable yet |

**Crossing.** Where the owner's policy is `CAllocator<P>`, the conversions
keep the pointer and the allocator and copy nothing:

- `CBox::from(Box<T, CAllocator<P>>)`, and back with `unsafe into_box`. A
  `Box` lends `&mut T`, through which safe code moves the object's bytes. So
  the caller of `into_box` vouches for three things:
  - the object's bytes are a valid `T` (C may have left a field unwritten, or
    written a `bool` of 2);
  - the object is not address-sensitive;
  - nothing else reaches the object.
- `Vec::from(CVec<T, CAllocator<P>>)`, for the elements that a `CVec` hands
  out as a slice.
- `CVec::try_from(Vec<T, CAllocator<P>>)`, which gives the `Vec` back
  unchanged if the shrink below fails.

C is never handed a pointer that was not allocated, and Rust is never handed
a C NULL:

- Zero-sized types are refused at compile time on every path ffibox offers:
  `try_box`, the policy impls and the conversions. The reason is that a `Box`
  of a zero-sized type, like every empty `Vec`, holds a dangling pointer.
  std's own `Box::new_in` and `Vec::new_in` accept zero-sized types, as they
  do for any allocator. This has two consequences:
  - A raw pointer taken out of a `Box` over `CAllocator` may go to C's free
    only for a non-zero-sized `T`, as `try_box` guarantees.
    `Box::into_raw_with_allocator` yields that pointer, and it is the only
    way a native-layout `T` reaches C.
  - A run goes to C through `CVec::try_from` and `into_raw_parts`, never as
    the raw parts of a `Vec`.
- An empty `Vec` becomes the NULL `CVec`. If the `Vec` has capacity, that
  capacity is freed.
- An empty non-NULL `CVec` (C's `malloc(0)`) is freed on the way to a `Vec`.
  The `Vec` would otherwise leak it at capacity 0.
- A `CVec` records only its length, so a `Vec`'s spare capacity is first
  given back through the family's `realloc`, before the `Vec` becomes a
  `CVec`. A free that takes a size is then given the buffer's size.

Hand C a buffer through the `CVec`'s `into_raw_parts`, not a `Vec`'s
`as_mut_ptr`, which is dangling when the `Vec` is empty.

**The allocator's rules.**

- Zero-sized requests never reach C.
- Alignment beyond the family's is an `AllocError`, and a compile-time
  refusal in `try_box`, the policy impls and the conversions.
- `grow` and `shrink` go through `realloc`. On failure the old block is
  untouched.
- No method unwinds.
- A buffer that C allocated with the family counts as allocated by
  `CAllocator<P>` once its ownership has passed to the caller.
- A layout fits such a buffer when its alignment is the family's or less and
  its size is one the family's free accepts. So where the free ignores the
  size, as `free` does, `Vec::from_raw_parts_in(ptr, len, len, a)` adopts a
  buffer whose capacity C never recorded.
- `Box::new_in` and `Vec::push` abort when the family fails, as std's do.
  `CAllocator::try_box` (std's unstable `Box::try_new_in`) and
  `Vec::try_reserve` return the failure.
