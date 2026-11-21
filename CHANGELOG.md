# Changelog

What changes between releases of ffibox, and what an upgrade asks of code
written against the release before. A change that compiles unchanged but
behaves differently, or that adds an obligation to an `unsafe impl`, has a
migration note here: nothing else reports it.

## Unreleased

Changes on `devel` since 0.1.1.

### Added

- **`CAtomicOwner::*_target` and `CPtrDeref`** (`atomic_owner` module).
  `get_target`, `try_insert_target`, `get_or_init_target` and
  `get_or_try_init_target` lend the owner's pointee (`&O::View`: `T` for
  `Box<T>` and for `CShared<T>` with `T: CShareable`, `CStr` for
  `CStrBox`) instead of `&O`, a reference to the field's own storage.
  They load the pointer atomically, so a field that one thread fills with
  compare-and-swap while others read it is never read non-atomically: the
  `&O` methods are not a data race in the Rust memory model (a failed
  compare-and-swap is a load) but ThreadSanitizer, which models every
  compare-and-swap as a write, reports them. Use the `*_target` methods for
  fields shared between threads; `try_insert_ptr` (any owner) fills the
  field and returns pointers only. `CPtrDeref` is a new unsafe trait
  (implemented for `Box`, `CShared<T: CShareable>` and `CStrBox`; not for
  `CBox`/`CArc`, reached through handles); nothing existing changes.
  `tests/counted.rs` reproduces the pattern; the README matrix runs it under
  ThreadSanitizer.
- **Counted Rust objects whose count C shares** (`counted` module, `alloc`
  feature). `CShared<T>` is `Arc<T>` for an object Rust allocates whose
  reference count is a field of its own, which C increments and decrements
  in place (libgit2's `git_refcount`, touched by `GIT_REFCOUNT_INC` and
  `git_odb_free`). The binding's `unsafe impl CCounted` states how the count
  is set when an object is adopted (`c_init`, which `CShared` calls, so no
  value's count is trusted), taken (`c_up_ref`, `false` before it could
  wrap, on which `Clone` aborts as `Arc`'s does), dropped (`c_down_ref`,
  with `Arc`'s `Release` / `Acquire`, also deciding whether an owner field
  keeps the object alive), proved sole (`c_is_sole_owner`, `Acquire`) and
  freed (`c_release`, by default the `Box<T, Alloc>` it lives in, `Alloc`
  zero-sized and stateless, checked at compile time); its contract carries
  `CRefClone`'s clauses for a C count. `unsafe impl CShareable` adds that a
  `&T` may live while C holds references, which gives `Deref` and, through
  `T`, `PartialEq` / `Eq` / `PartialOrd` / `Ord` / `Hash` / `Display` /
  `Borrow<T>`. `CShared` is `#[repr(transparent)]` over the pointer, `Send`
  / `Sync` on `Arc`'s terms, and has `from_box` (and `From<Box<T, Alloc>>`)
  / `try_new`, `from_raw` / `from_borrowed` for references crossing to C,
  and, as associated functions like `Arc`'s, `as_ptr`, `into_raw`,
  `get_mut`, `ptr_eq`, `increment_count` / `decrement_count`, and
  `release`, C's `foo_free`, which the binding's export calls inside its
  panic barrier. It is the outbound counterpart of `CArc`, which counts
  objects C allocates and reaches through handles. Nothing existing
  changes. See
  [docs/outbound.md](docs/outbound.md#counted-objects-published-fields-and-bitfields).
- **Owners in C pointer fields filled once** (`atomic_owner` module).
  `CAtomicOwner<O>` is a C struct's pointer field, `#[repr(transparent)]`
  over `AtomicPtr<O::Target>`, holding an owner that is one pointer
  (`CPtrOwner`: `CBox`, `CArc`, `CStrBox`, `CShared`, `Box`), with
  `OnceLock`'s methods: `get`, `set`, `try_insert` (which hands the loser
  back), `get_or_init`, `get_or_try_init`, `take`, `get_mut`,
  `into_inner`. Two threads may both create a value; the loser of the
  compare-and-swap drops its own, as C's does. `as_ptr` is C's weak
  pointer, and `unsafe replace` is C's setter, whose caller states that
  every borrow of the old value has ended. All-zero bytes are an empty
  field. `Sync` when `O: Send + Sync`. `CPtrOwner` is a new unsafe trait,
  implemented for ffibox's pointer owners under a `Default` policy; nothing
  existing changes.
- **C bitfields by field** (`bitfield` module). `CBits<W>` names a field of
  a unit `W` (`u8` to `u64`) once, in C's declaration order:
  `CBits::first(width)` then `.next(width)`, which place the bits as System
  V, AAPCS and Microsoft x64 do for the target's endianness, or `CBits::at`;
  a field that does not fit, or has no bits, fails to build. `CBitfield<W>`
  is the unit, `#[repr(transparent)]` over `W`, with `get` / `set`
  (truncating as C's assignment does) / `is_set` / `set_flag`.
  `CAtomicBitfield<W>` is the unit shared between threads: `load` / `store`
  / `swap` / `is_set` / `set_flag`, each change one atomic
  read-modify-write that keeps the other fields; `set_flag` skips the write
  when the flag is already as asked only for an ordering with no release
  part. `CAtomicBitfield<W>` needs `W: CAtomicBitWord`, which `u64` is
  not on i686 (`AtomicU64` is more aligned than C's `long long` member), so
  the type itself is rejected there. Which bytes the unit is follows the ABI: on System V and
  AAPCS it covers the bytes the run occupies, not the declared type. The
  layouts are checked against clang for seven targets
  (`tests/bitfield_c_layout.rs`). Unsigned fields only.
- **C tables implemented in Rust** (`vtable` module, `alloc` feature).
  `CTable<T, S>` holds Rust state `S` behind a C table of function pointers
  of the layout type `T`, which C calls into: the table first, each slot
  recovering the state under the receiver it declares (`&S` shared, `&mut S`
  exclusive, a refusal for re-entry that would alias), behind the panic
  barrier with its declared result, and released once through its `release`
  slot, whoever releases it and whenever. `impl_cvtable!` mints the slots and
  the `CSlots<S>` impl that installs them, beside the binding's own
  `unsafe impl CVtable<S> for T {}`, whose `SAFETY` comment states where C
  passes each slot its table and that no slot C calls is left NULL.
  `impl_clocal_handles!` marks a layout type whose handles are checked to
  be neither `Send` nor `Sync`, so its handles are lent over any state. Tables were hand-written before:
  nothing existing changes, and a hand-written one keeps working. Moving one
  to `CTable` replaces its `#[repr(C)]` struct, trampolines, `free` and
  re-entry state with one `impl_cvtable!` over the trait it already has; see
  [docs/vtables.md](docs/vtables.md).
- **`Box` and `Vec` over a C allocator** (`allocator` module; `758eaaf`,
  `155b9df`). `CAllocator<P>` is a `CLenAlloc` policy, a C allocation
  family, as the standard `core::alloc::Allocator`, so
  `Box<T, CAllocator<P>>` and `Vec<T, CAllocator<P>>` allocate, grow and
  free through C's `malloc` / `realloc` / `free`, and adopt what C allocated
  with them; it is an `Allocator` over a `Copy` policy, which has no drop
  to unwind. It is zero-sized and `Copy` over a unit-struct policy, so a box
  stays one pointer; zero-sized requests never reach C, alignment beyond the
  family's is an `AllocError`, and its docs say which C buffers it counts as
  allocated and which layouts fit them. It is also the storage-only policy
  over the family for `CBox`, `CStrBox` and `CVec`, and `CBox` / `CVec`
  over it convert to and from `Box` / `Vec` without copying: `CBox::from`
  and `unsafe CBox::into_box` (its caller vouching that the object is a
  valid, movable `T` nothing else reaches), `Vec::from` and
  `CVec::try_from`.
  `CAllocator::try_box` is `Box::try_new_in`, which std has not stabilized.
  `CLenAlloc` gains a provided `c_realloc_len`, C's `realloc`, as
  `c_alloc_len`, a copy and `c_drop_len`; `impl_clenalloc!` binds a
  family's `malloc`, with `align =` and `realloc =`. Nothing existing
  changes: a hand-written `CLenAlloc` keeps the provided `c_realloc_len`,
  which its contract already makes sound, and yields its `CAllocator`
  as it is. See [docs/ownership.md](docs/ownership.md#box-and-vec-over-a-c-allocator).
- **Doc aliases on `impl_cvtable!` slots.** A slot's doc comments may be
  followed by `#[doc(alias = "…")]` attributes, which the slot's trampoline
  carries: a port names the C function a slot stands in for, as its
  conventions ask of every item that replaces one. The trampoline still
  takes no other attribute. Nothing existing changes. `tests/vtable.rs`
  (`aliases`) covers it.
- **A restricted visibility for `define_ctype!`.**
  `define_ctype!(pub(crate) Foo, FooRef, FooMut, ffi::foo_st)` gives the
  layout type and its two handles that visibility instead of `pub`, so
  that a crate may mirror a C type it keeps crate-private: the `CCell`
  impl names `$c_type` as `CCell::C`, which a `pub` type may not do for a
  less visible one (E0446). Without a visibility the types are `pub`, as
  before. `tests/ctype_visibility.rs` covers it.

### Migration from 0.1.1

- **`CVec` drops its elements** (`936204b`). A `CVec` now drops each element,
  as a `Vec` does, before its policy frees the buffer; in 0.1.1 it freed the
  buffer alone. Code written for 0.1.1 that released the elements itself
  still compiles, and now frees each element twice:
  - **A `Drop` that drops a `CVec`'s elements**, on a wrapper that holds one
    (`ptr::drop_in_place(v.as_mut_slice())`, or a loop over the slots before
    the `CVec` frees the buffer): delete the part that touches the elements.
    The `CVec` drops them, then frees the buffer.
  - **A `CLenDrop` whose routine frees the elements with the array**
    (`foo_array_free(arr, n)`), over elements that own: make the elements
    owners, `Option<CBox<Foo, FooFree>>`, `Option<CStrBox<P>>` or
    `CVal<Foo, P>`, under a policy that frees the buffer alone. Where the
    routine does more than those frees, wrap the array by hand, as
    [docs/hand-written.md](docs/hand-written.md#an-array-freed-by-one-routine)
    describes. Over bare pointers, which own nothing, the routine still frees
    each element once, but safe code can store any pointer in a slot through
    the `&mut [*mut foo]` the `CVec` hands out, so the same change applies.

  Elements that own nothing, such as integers, raw pointers and
  `MaybeUninit`, drop to nothing, so a `CVec` of them behaves as before. To
  find the code to change, check every `Drop` impl on a type that holds a
  `CVec` of owners, and every `CLenDrop` (or `impl_clendrop!` routine) that
  reads the elements it frees.
- **`CDupClone<c_char>` copies any string** (`936204b`).
  `CStrBox::from_c_str` hands `c_dup` a Rust-side `CStr`, so a
  `CDupClone<c_char>` must copy any live NUL-terminated string, reading it
  only, as `strdup` does. A routine that takes only strings from its own
  allocator, such as an up_ref on a counted string, no longer meets the
  contract: drop that `CDupClone` impl and copy through a routine of the
  wrapper's own.

### Changes the build reports

- **Rust 1.100 is the minimum** (`f944c17`), the first release with the
  standard `Allocator` interface; an older toolchain refuses the crate.
- **A policy with state no longer fits `CBox`, `CArc` or `CStrBox`**
  (`2398db0`), which are now `#[repr(transparent)]` over their pointer. Such
  a policy fails at monomorphization, so `cargo build` reports it where
  `cargo check` does not. Hold the object in a `CBoxState`, adopted with
  `from_raw_with`; a counted object or a string under such a policy has no
  stateful owner yet.

### Additions

- **`define_ctype!` takes lifetimes** (`9c1a54c`).
  `define_ctype!(Reference<'r>, ReferenceRef, ReferenceMut, ffi::git_reference, phantom = &'r ())`
  declares `Reference<'r>`, `ReferenceRef<'a, 'r>` and `ReferenceMut<'a, 'r>`,
  `CCell` for every `Reference<'r>` and the seams, the triple a
  lifetime-carrying layout type wrote by hand. `phantom` decides the
  variance; the macro adds the `*const ()` itself, so the type stays `!Send`
  / `!Sync`. Lifetimes only: a layout type generic over a type is still
  hand-written. The form without lifetimes expands as before.
- **`impl_cdupclone!` and `impl_cguarded!` take a leading lifetime list**
  (`9c1a54c`), `impl_cdupclone!(<'r> ReferenceFree, Reference<'r>, f)`, for a
  layout type with a lifetime, which neither took before: the deep copy and
  the lock over such a type need no hand-written impl. `impl_cguarded!`'s
  locked handle is then `FooLocked<'a, 'r>`.

  A hand-written lifetime-carrying triple is the same layout and keeps
  compiling; moving one to the macro is optional. Moving it, mind that the
  macro adds `as_ref` / `as_mut` on the layout type, as it does for every
  `define_ctype!` type, which clash with inherent methods of those names; that
  a marker without `*const ()` over a C type that is `Send` loses `Send` and
  `Sync`, which then take an `unsafe impl`; and that `Debug` stays the
  wrapper's.
