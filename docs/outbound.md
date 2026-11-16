# Outbound

Outbound is Rust's side of the boundary: memory that Rust owns and C holds,
calls or reads. The inbound documents, [ownership.md](ownership.md) and
[borrows.md](borrows.md), cover memory that C owns and Rust holds. This
document walks the other way, with the same terms.

You need ffibox only where C **aliases** a value: C keeps a pointer to it
after a call returns, frees it, or calls back into it. A pointer that you pass
down for one call, and that C does not keep, is plain Rust:

- `&mut x as *mut _` for C that writes;
- `&x as *const _` for C that only reads; or
- a `&CStr` or a slice's `as_ptr()`.

## The primitives

| C does | Primitive | Rust counterpart |
|--------|-----------|------------------|
| frees an object or buffer that Rust allocated | an owner under `RustAlloc`, surrendered with `into_raw` / `into_c` | `Box::into_raw` |
| frees a string or buffer with its own allocator | `CStrBox::from_c_str`, `CVec::from_slice`, `CVec::from_fn`, `CVec::new_uninit` under that allocator's policy | `CString`, `Vec` |
| frees a value or buffer that Rust built, and grew, in C's allocator | `Box<T, CAllocator<P>>` / `Vec<T, CAllocator<P>>`, converted into a `CBox` / `CVec` and surrendered with `into_c` / `into_raw_parts` | `Box::new_in`, `Vec::new_in` |
| calls a closure during one call | `CClosure`: `ctx()` and `trampoline()` | `&mut F` |
| keeps a closure or value, and releases it later | `CPayload`: `into_raw()`, `release_fn()`, `trampoline()` | `Box<F>` |
| hands Rust a function and its context to call | `CFn` for the call or borrow that the pair came with, or `CFnBox` with the routine that releases the context: `call()`, `parts()` / `into_raw()` | a closure |
| holds a callback that is either a Rust closure or a C function | `CCallback` (for a call or borrow) or `CCallbackBox` (kept until released): `call()` from Rust, `c_form()` / `into_c()` to C | `impl FnMut`, `Box<dyn Fn>` |
| calls into a table of function pointers, where each slot is passed the table (a backend, a stream) | `CTable<T, S>`, with its slots from `impl_cvtable!`: `as_c_ptr()` / `into_c()`, released through its `release` slot | `Box<dyn Trait>` |
| takes and drops references to an object Rust allocated, through a count field of its own (`GIT_REFCOUNT_INC`, `foo_free`) | `CShared<T>` over a `CCounted` type (`alloc` feature): `clone()`, `into_raw()` / `from_raw()`, `release` behind C's free | `Arc<T>` |
| reads a pointer field that Rust fills on first use, or fills it itself with compare-and-swap (`repo->_odb`) | `CAtomicOwner<O>` over a one-pointer owner: `get_or_init()`, `try_insert()`, `as_ptr()`, `unsafe replace()` | `OnceLock<O>`, `AtomicPtr` |
| reads or writes bitfields of a struct Rust mirrors | `CBitfield<W>` / `CAtomicBitfield<W>`, fields named by `CBits<W>` | the integer's shifts and masks, `AtomicU32::fetch_or` |
| returns an object through an out-parameter | `CBox::from_c_out`, `CStrBox::from_raw_out` | a return value |
| calls a Rust routine that must not unwind into it | `callback::barrier` | `catch_unwind` |
| frees, copies or constructs a ported type's objects | `export_cdrop!`, `export_cdupclone!`, `export_cnew!` | the C routines they replace |

## Choosing a primitive

- **C frees it.** The memory comes from the allocator that C frees with.
  - If that allocator is C's own (`free`, `lib_free`), build the memory
    through that allocator's policy:
    - `CVec::from_fn` and `CVec::new_uninit` with a `CLenAlloc` policy;
    - `CVec::from_slice` and `CStrBox::from_c_str` with a copying policy; or
    - a `Box` / `Vec` over `CAllocator<P>` when Rust builds or grows it
      natively, converted into a `CBox` / `CVec` to hand over.
  - If C frees it through a Rust routine, allocate with `RustAlloc` and offer
    the release with `export_cdrop!`.

  Then surrender it with `into_raw`.
- **C calls back during the call** (`foreach`, `qsort_r`). Use
  `CClosure::new` over `&mut F`.
  - The trampoline's type is inferred from the C declaration that it is
    passed to.
  - The closure's arguments are the C types. The binding converts them to
    handles before it calls its consumer's closure.
  - With `std`, a panic stops the C routine through the failure value, and
    resumes after the routine, at `finish()`. A re-entrant invocation is
    refused the same way.
  - `F` must be `Send`, unless the routine calls back on the calling thread
    only (`unsafe CClosure::new_local`).
- **C keeps the callback** (`set_log_callback(cb, ctx, free)`). Use
  `CPayload` over an `Fn` closure. Surrender it with `into_raw`; C releases
  it through `release_fn()`. C may call it from several threads or
  re-entrantly, so it is reached as `&F`, and `into_raw` requires
  `Send + Sync`. State that it changes goes behind a `Mutex` or an atomic. A
  panic in it aborts, so a fallible body goes in `barrier`.
- **C keeps a value** (a container's payload, user data). Use `CPayload<T>`,
  and `CPayload::borrow` inside the callbacks that receive it.
- **C hands Rust a callback** (an options struct's `progress_cb` and
  `payload`, a ported function's `cb, ctx` arguments through its export). Use
  `CFn`.
  - `CFn::new` is `unsafe`: its contract states what the C that supplies the
    pair guarantees.
  - `call((args…))` takes C's argument types, with the context left out. The
    binding's safe callable therefore converts its arguments and calls in one
    justified block.
  - `parts()` hands the pair to another C routine unchanged, with no
    trampoline between.
- **A callable is either** (a field that a consumer sets with a closure and C
  sets with a function, a callback that ported code calls whoever supplied
  it). `CCallback` / `CCallbackBox` hold whichever it is.
  - Called from Rust, a closure runs through its trampoline, with the same
    refusal and panic handling as when C calls it.
  - Handed to C, a C function goes as its own pair.
- **C calls into a table Rust implements** (`git_config_backend`, a
  `git_writestream`: a struct of function pointers, each passed the table).
  Use `CTable<T, S>`: the state `S` behind the table of the layout type `T`.
  `impl_cvtable!` mints the table's slots from the trait that `S` implements.
  Each slot declares its receiver, `&S` or `&mut S`, which decides what
  re-entry the slot allows, and its results for a panic and a refusal. C
  releases the table through the slot declared `release`.
  [vtables.md](vtables.md) walks through it.

  A family of hooks that C calls with no pointer back (`git_allocator`) is
  not a table. It is trampolines generic over the implementing type, in
  `barrier`.
- **C counts references to a Rust object** (a ported `git_odb`, whose
  `git_refcount` C's macros still increment, and whose `git_odb_free` C
  still calls). Implement `CCounted` for the type: `c_up_ref` and
  `c_down_ref` over its count field, with C's meaning (an owner field
  included), and `Alloc`, the allocator it lives in, or a `c_release` of
  its own. Hold references as `CShared<T>`. Then:
  - if every field C writes, and every field holders write while it is
    shared, is an atomic or behind the type's own lock, also implement
    `CShareable`: `CShared<T>` derefs to `T`, as `Arc<T>` does;
  - otherwise (C writes a plain field, `index->entries`), reach it through
    `as_ptr()` and the binding's handles, and `get_mut()` when the count
    proves the reference sole;
  - C's free is a one-line export over `CShared::release`, and a pointer C
    returns or lends is adopted with `from_raw` or `from_borrowed`.
- **C reads a pointer field that is filled once** (a repository's `_odb`,
  `_config`, `_refdb`, `_index`; a lazily built cache). Make the field a
  `CAtomicOwner<O>`, where `O` is the owner of what it points to (`CShared`,
  `CArc`, `CBox`, `CStrBox`, `Box`):
  - `get_or_init` / `get_or_try_init` create it on first use; a thread that
    loses the race drops its own value, as C's compare-and-swap loser
    frees. Where the loser must undo something first (an owner it set),
    `try_insert` hands it back;
  - `get()` borrows it for as long as the struct, C's weak pointer;
    `as_ptr()` is that pointer for an export to write out;
  - a setter (`git_repository_set_odb`) is `unsafe replace`, whose caller
    states that no weak borrow of the old value is used afterwards: the
    contract C's own weak pointers already have.
- **C reads or writes a bitfield** (`unsigned on_disk:1, dirty:1;`). Name
  each field once with `CBits<W>`, in C's order: `CBits::first(1)`, then
  `.next(width)` for each later field; the bits follow the target's ABI.
  The storage unit is a `CBitfield<W>` in the `#[repr(C)]` mirror, or a
  `CAtomicBitfield<W>` where several threads update it: each change is
  one read-modify-write that keeps the other fields, where a load and a
  store of the word would lose a concurrent change to another field.
- **C returns something through an out-parameter.** Use `from_c_out` /
  `from_raw_out`. The slot starts NULL and is adopted whatever the routine
  returns, so an object handed over on failure is still released.
- **C still frees a type Rust now allocates** (a port in progress). Use
  `export_cnew!` / `export_cdrop!` / `export_cdupclone!` with the type's
  policy, in the ported module's `ffi_export` gateway: the `mod ffi_export`
  that holds the routines the module offers to C. C then allocates and frees
  through Rust's allocator, and never through `malloc` / `free` crossed with
  it.

## Counted objects, published fields and bitfields

**`CShared<T>`** is `Arc<T>` whose count is a field of `T`, which C
increments and decrements in place. What `CArc` is for an object C
allocates, `CShared` is for one Rust allocates: the object is a Rust struct,
reached as `&T`, not through handles. The binding's `unsafe impl CCounted`
states, once, what the field means: how a reference is taken and dropped,
when the object must go, how it is freed. libgit2's `git_refcount` also
records an owner, which keeps the object alive past its last counted
reference. That logic stays in `c_down_ref`, which answers "release now"
only when the count is spent and no owner holds the object. `CShareable`
states that a `&T` may live while the object is shared: what C writes is
atomic. Without it, `CShared` gives a pointer and `get_mut`, never a `&T`.
`Send` and `Sync` follow `Arc`'s terms. A lifetime the object keeps (a
repository it borrows) goes in the binding's wrapper,
`struct Odb<'r>(CShared<GitOdb>, PhantomData<&'r Repo>)`, which then
derives `Clone`.

**`CAtomicOwner<O>`** is a C pointer field that holds an owner, with
`OnceLock`'s methods for filling it once and C's compare-and-swap for the
race. It is `#[repr(transparent)]` over `AtomicPtr<O::Target>`, so a mirror
keeps C's layout, and C may read the field, or fill it when NULL, as it
did. Safe code fills only an empty field, so a borrow from `get` lives as
long as `&self`. Replacing a full field invalidates every borrow, Rust's and
C's weak pointers alike, so `replace` is `unsafe`. `O` is any owner that is
one pointer (`CPtrOwner`). It is `Sync` when `O: Send + Sync`, as `OnceLock`.

**`CBits<W>`, `CBitfield<W>`, `CAtomicBitfield<W>`** read and write C
bitfields by field. Which bits a field occupies is the ABI's: on System V
and AAPCS, the first field declared takes the least significant bits of its
unit on a little-endian target, the most significant on a big-endian one.
`CBits::first` and `next` encode that rule, so the binding states the
declaration and not the masks. `CBits::at(lsb, width)` states a position
the declaration order does not give (after a `:0`). A field that does not
fit its unit fails at compile time. Assignment truncates to the field's
width, as C's does. Only unsigned fields are covered. A layout test against
the C compiler still checks the unit's offset and the bit positions.

## Conventions

- **Surrendering is safe, adopting is `unsafe`**, inbound and outbound alike:
  `into_raw` gives a pointer away, `from_raw` asserts what it addresses.
- **What one allocator allocates, only it frees.** On the Rust side, the
  policy in the owner's type enforces this. On C's side, an export enforces
  it by naming the policy.
- **No unwinding into C.** A panic that reaches an `extern "C"` function
  aborts. `barrier`, and `CClosure` with `std`, turn the panic into a value
  first.
- **Stable addresses.** A value that C keeps a pointer to lives behind a
  pointer that does not move while C holds it: a `CBox`, a `CPayload`, a
  `CClosure` borrowed for the call. A by-value `Foo` or `CVal` moves, so C
  must not keep a pointer into one.
