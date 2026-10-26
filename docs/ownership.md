# Ownership

An owner is a Rust value whose drop releases a C object. Which owner you reach
for follows from the shape of the thing owned; *what* its drop does is a
separate choice, the [policy](#policies--what-teardown-means), and the owner's
type carries both.

For the borrowed side — handles over objects C or a parent releases — see
[borrows.md](borrows.md).

## The owners

| Scope | Type | From | Role |
|-------|------|------|------|
| owners | `CBox<Foo, P>` | ffibox | the sole owner of an object behind a pointer, released by `P` on drop; `Clone` when `P` deep-copies; built safely by `new` / `new_zeroed` / `new_uninit` when `P` constructs |
| | `CVal<Foo, P>` | ffibox | an owned value held inline, its resources disposed by `P` on drop |
| | `CStrBox<P>` | ffibox | an owned NUL-terminated `char *`; read-only `CStr` / `str` / byte views |
| | `CVec<T, P>` | ffibox | an owned `(ptr, len)` array, NULL when empty; `&[T]` for plain elements, `CSlice` for wrapped C objects; `from_slice` copies a Rust slice into `P`'s allocator |
| shared owner | `CArc<Foo, P>` | ffibox | one counted reference to a refcounted object; `Clone` through the up_ref; the shared handle always, `get_mut` / `make_mut` when the count proves it sole or after a copy, and `lock()` → `CGuard` when `Foo` is `CGuarded` |

## Conventions

- **Adopting a pointer is `unsafe`; giving one out is not — as on `Box`.**
  `from_raw*` / `from_c*` are `unsafe`, since they assert ownership of what the
  pointer addresses; `into_raw*` / `into_c` and `as_ptr` / `as_c_ptr` are safe,
  since a raw pointer asserts nothing until someone dereferences it. The
  owners' `from_raw` / `into_raw` / `as_ptr` speak the Rust-side pointee;
  `CBox`'s `from_c` / `into_c` / `as_c_ptr` speak the C type, so C interop is
  cast-free. The handles have seams of their own — see
  [borrows.md](borrows.md#conventions).
- **Constructing through the policy is safe.** `new`, `new_zeroed` and
  `new_uninit` (each with a `_with(policy)` form) allocate through the routine
  the owner frees with, and return `None` when C fails, as `from_raw` does on
  NULL. The one `unsafe` step is `assume_init`, which promotes storage C has
  filled through `as_c_ptr`. There is no safe `write(value)`: a C object is
  initialised where it lives.
- **`from_raw*` adopts ownership; `from_ptr*` borrows.** Pick the verb by what
  you return.
- **Thread safety is opt-in for wrapped C types, opt-out for raw memory.**
  `Foo` carries a zero-sized marker withholding `Send` / `Sync`; `CBox`,
  `CVal`, `CVec` and the views over a `Foo` inherit them once the wrapper
  writes `unsafe impl Send / Sync for Foo` with a safety proof, on the terms
  of their std counterparts — `CBox` like `Box`, `CArc` with
  `Foo: Send + Sync` like `Arc`, and `CGuardedRef` with `Foo: Send` like
  `&Mutex<_>`.
  Owners that never name a `Foo` — `CStrBox` and a `CVec` of
  plain elements (`CVec<u8, _>`) — follow their policy alone, and a unit-struct
  policy is `Send + Sync`, so **they cross threads by default**. That is right
  for an allocator any thread may free into (`free`, a library's
  `lib_free`); for one that must free on the allocating thread, opt out by
  giving the policy a `PhantomData<*const ()>` field.

## Choosing an owner: what shape is the owned thing?

Reached once dropping the Rust value is what releases the object. When it is
not, the answer is a borrow instead — see
[borrows.md](borrows.md#choosing-a-borrow).

- **A NUL-terminated string** (`char *`, no stored length) → `CStrBox<P>`.
  `strlen` recovers the length, so one `CDrop<c_char>` policy covers a plain
  free and a clearing free alike. Its views are read-only, like `CStr`.
- **A counted array** → `CVec<T, P>`, one policy per allocator family. Plain
  elements (`CPlainElem`: integers, floats, raw pointers, `MaybeUninit`) read out as
  a real `&[T]` because the buffer is exclusively owned; wrapped C objects come
  out as handles through `as_handles` → `CSlice`, since `&[Foo]` would cover
  them. Where C returns NULL with 0 for an empty array, adopt with
  `from_raw_parts_or_empty`; plain `from_raw_parts` treats every NULL as
  failure.
- **One object held inline** — a local, a field, an array element, with no
  pointer of its own:
  - owns no resources → the `Foo` itself (`Foo::zeroed()` when it is
    `CZeroable`, or a wrapper constructor that has C fill a local);
  - owns resources and stands alone → `CVal<Foo, P>`;
  - embedded in a parent C struct → a bare `Foo`, disposed by the parent's
    teardown (a `Drop` on it would dispose twice);
  - address-sensitive (points into itself, or C recorded its address) → not
    inline at all: behind a pointer, as below.
- **One object behind a pointer** → `CBox<Foo, P>` if this is the only
  reference, `CArc<Foo, P>` if it is one of several counted ones. `CBox` owns
  only `define_ctype!` layout types, as does `CArc`: a `void *` payload is
  bytes (`CVec<u8, P>`) or a typed owner of its own.

## Policies — what teardown means

Every owner's destructor is a **policy**: a type you declare, usually a unit
struct deriving `Clone, Copy, Debug, Default`, implementing the lifecycle
traits. Bind a C routine to it with the `impl_*!` macros, or implement the
traits by hand when teardown needs runtime state. A unit struct is
`Send + Sync`, which makes a `CStrBox` or plain-element `CVec`
under it free on whichever thread drops it; a thread-bound allocator's policy
carries `PhantomData<*const ()>` instead (see
[Thread safety](#conventions) above).

| Trait | Defines | Macro | Drives |
|-------|---------|-------|--------|
| `CDrop<T>` | `c_drop` — a `*_free`, or a refcount down-ref | `impl_cdrop!(P, Foo, f)`; `_str` for `c_char` | `CBox`, `CStrBox`, `CArc` |
| `CDupClone<T>: CDrop<T>` | `c_dup` — a deep copy (a NEW pointer, NULL on failure) | `impl_cdupclone!(P, Foo, f)`; `_str` for `strdup` | `Clone` / `try_clone` |
| `CRefClone<T>: CDrop<T>` | `c_up_ref` — a refcount increment on the SAME pointer; `c_is_sole_owner` (default `false`) | `impl_crefclone!(P, Foo, f)`; `…, ok = |r| r == 1`, `…, sole = g` | `CArc`'s `Clone`, `get_mut`, `make_mut` |
| `CLenDrop` | `c_drop_len` — a buffer free, given the byte length | `impl_clendrop!(P, f)` | `CVec` |
| `CLenClone: CLenDrop` | `c_clone_len` — a buffer memdup (`T: Copy` only) from any readable source; `ALIGN` (default 1) bounds the element types it can copy | `impl_clenclone!(P, f)`, `impl_clenclone!(P, f, align = 16)` | `CVec`'s `Clone`, `CVec::from_slice`, `CSlice::to_cvec` |
| `CDispose<T>` | `c_dispose` — `*_uninit` / `*_clear` on a value | `impl_cdispose!(P, Foo, f)` | `CVal` |
| `CNew<T>: CDrop<T>` | `c_new` — a zero-argument `*_alloc` / `*_new` | by hand | `CBox::new`, `CArc::new` |
| `CAlloc<T>: CDrop<T> + CDrop<MaybeUninit<T>>` | `c_alloc` — uninitialised storage (`malloc`); the free must not read it | by hand | `CBox::new_uninit` |
| `CAllocZeroed<T>: CDrop<T>` | `c_alloc_zeroed` — zero-filled storage (`calloc`) | by hand | `CBox::new_zeroed`, `CArc::new_zeroed` |
| `CZeroable`, on `Foo` | all-zero is valid for every accessor, safe wrapper and paired teardown; provides `Foo::zeroed()` | `unsafe impl CZeroable for Foo {}` | `new_zeroed`, `Foo::zeroed()` |
| `CGuarded`, on `Foo` | `c_lock` / `c_unlock`; the `Locked` handle; the `Scope` it covers | `impl_cguarded!(Foo, FooLocked, lock = f, unlock = g)`; `…, ok = |r| r == 1`; `impl_cguarded!(Foo, all, …)` for `CGuardedAll` | `CArc::lock`, `CGuardedRef::lock` |

**Because the policy is a type parameter, an owner cannot exist without a
destructor, and one C type can have several.** `CBox<Foo, FooFree>` next to
`CBox<Foo, FooUnref>`: exactly one ever runs per box, chosen by its type. And
since each clone trait sits on the same policy as the drop trait, a `*_dup` is
always settled by its `*_free`.

**`CBox` is a sole owner; `CArc` shares.** `CBox::as_mut` hands out the
exclusive handle, which is sound only while nothing else uses the object, so
`CBox` clones only by deep copy. A refcounted object fits a `CBox` while the
box holds its only reference (with the down-ref as `c_drop`); `CArc::from`
shares it, and `CBox::try_from` (or `try_into_box`) takes it back once the count
is 1 again.

**Shared and mutable means locked — usually, part of it.** A `CArc` has no
unlocked write path: its exclusive handle needs the count to prove the
reference sole (`get_mut`, which needs `c_is_sole_owner` — an Acquire read of
a count that covers every reference, C's own included), or a private copy
(`make_mut`, through `CDupClone` — never a byte copy, which would duplicate
the object's sub-allocations, its count and its lock). A C object rarely locks
all of itself: it locks the state its holders mutate — a cache, a list of
backends — and leaves the rest fixed once set up. That is `Arc<T>` with a
`Mutex` field, not `Arc<Mutex<T>>`, and the handles split the same way:

| State | Handle | Reached through |
|-------|--------|-----------------|
| fixed once shared; routines that lock internally | `FooRef` | `CArc::as_ref()`, always |
| protected by the lock — reads too; routines that need it held | `FooLocked` | `CArc::lock()` → `CGuard::as_locked()` |
| anything | `FooMut` | `CArc::get_mut()`, sole reference only |

A getter for protected state never goes on `FooRef`: it would race with a
writer holding the lock through another clone. `CGuarded`'s contract makes C
take or require the same lock, and makes the layout type invariant in any
lifetime the protected state can hold (as a `Mutex` field does), so a covariant
`CArc` cannot shrink one. The lock must not be recursive: a second `lock` on
the same thread blocks forever, or panics when the lock reports the
self-deadlock (`EDEADLK`), rather than hand out a second `FooLocked`. A failing
C lock call panics, as `std`'s locks do; there is no poisoning. An object whose
lock covers all of it is `CGuardedAll` (`impl_cguarded!(Foo, all, …)`): its
locked handle is `FooMut`, so it is reached only through a `CGuardedRef`,
which has no unlocked path, and `CArc::lock` rejects it at compile time.

**Routines are plain paths, type-checked.** Each macro calls the routine with a
pointer of the exact C type; a routine for the wrong type does not compile.
Where a call can fail — an up_ref, a lock — the routine either returns `()` or
is bound with `ok = |r| r == 1` (whatever its success value is); a status code
is never silently discarded, so `pthread_mutex_lock`'s `EDEADLK` becomes a
panic rather than a second locked handle. A
destructor of any other shape — one taking the slot (`ffi::foo_free(&mut p)`),
a `void *` allocator free, extra arguments — goes behind a small `unsafe fn`
adapter passed by path. Teardown is unconditional: a gate that suppresses it on
some paths folds into the routine itself.

**Runtime-state policies.** When teardown needs a value chosen at the wrapping
site (`list_pop_free(list, elem_free_fn)`), write the policy by hand — a
struct holding the state, implementing `CDrop<T>` (and `CDupClone<T>` + `Clone`
to clone). Without `Default` there is no `from_raw(ptr)`; adopt with
`from_raw_with(ptr, ElemFree(f))` and release with `into_raw_with()`. The box
then carries the state and is no longer pointer-sized.

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

**The construction phase.** An allocation that is still being filled in is held
under a storage-only policy, either `new_uninit` or a hand-written
`CBox<Foo, StorageFree>`, and then promoted with `with_policy(FooFree)` once it
satisfies the full destructor. Bail with `?` before promoting and only the
storage is freed.

## Choosing a policy

For boxes, strings and buffers.

| C routines | Policy |
|------------|--------|
| `foo_free` | `impl_cdrop!` |
| `foo_alloc()` + `foo_free` | `impl_cdrop!` + a hand-written `CNew` → `CBox::new()` |
| `malloc` / `calloc` + `free`, for any type | a generic hand-written `CDrop<T>` + `CAlloc<T>` / `CAllocZeroed<T>` |
| `foo_free` + `foo_dup` | `impl_cdrop!` + `impl_cdupclone!` |
| `foo_unref`, held as the only reference | `impl_cdrop!` with the down-ref; no `Clone` |
| `foo_unref` + `foo_up_ref`, shared, read-only | `impl_cdrop!` + `impl_crefclone!` → `CArc` |
| … and a readable count | `impl_crefclone!(…, sole = g)`: `get_mut` without a copy |
| … with state every holder mutates under a C lock | + `impl_cguarded!(Foo, FooLocked, …)` → `CArc::lock`; protected getters and setters on `FooLocked` |
| a global, never freed, under one C lock | no policy: `impl_cguarded!(Foo, all, …)` → `CGuardedRef<'static, Foo>` |
| a counted reference with no up_ref | `impl_cdrop!` alone → `CArc` without `Clone` |
| teardown needs runtime state | a hand-written policy; adopt with `from_raw_with` |
| built in place, not by a C constructor | `CAlloc` → `new_uninit` → `assume_init`, then `with_policy` if a fuller destructor takes over |
| destructor of another shape | an `unsafe fn` adapter, passed by path |

An owned pointer crosses the FFI boundary on the owner's raw seam: `into_raw`
surrenders it to C, `from_raw` adopts one C allocated.
