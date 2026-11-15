# Hand-written wrappers

Only two things need this layer:

- a wrapper that `define_ctype!` cannot express, which is a layout type
  generic over a type; and
- an owner that no policy expresses:
  [an array freed by one routine](#an-array-freed-by-one-routine) that does
  more than free its elements and itself.

A layout type that carries only lifetimes is not hand-written. `define_ctype!`
declares it,
`define_ctype!(Blob<'r>, BlobRef, BlobMut, ffi::blob, phantom = &'r Repo)`,
as [examples.md](examples.md#a-layout-type-with-a-lifetime--define_ctype-with-phantom)
shows. One that is already written by hand is the same layout. It moves to the
macro when its module is next touched, or it stays.

A C table that a library calls into and Rust implements (a backend, a stream:
a struct of function pointers, each passed the table) is not hand-written
either. `CTable`:

- lays the table out with its state;
- recovers the state in each slot;
- guards re-entry;
- puts each slot behind the panic barrier; and
- releases the table once.

`impl_cvtable!` mints the slots, as [vtables.md](vtables.md) describes. Only a
slot that the macro cannot express is written by hand. Write it as an
`unsafe extern "C" fn` whose body is `CTable::enter`, in a hand-written
`CSlots<S>` impl beside an `unsafe impl CVtable<S>`.

For a hand-written wrapper, you write by hand the
layout type, its handles and their seams, against the items in the table
below. You do not write its policies by hand, unless no macro takes them:

- **A layout type generic over a type** (`Stack<T, S>`) has no elided form,
  so each of its policies is an `unsafe impl<T, S>` by hand.
- **A hand-written layout type with a lifetime** takes every policy macro, as
  a `define_ctype!` one does. `impl_cdrop!(BlobFree, Blob<'_>, ffi::blob_free)`
  implements `CDrop<Blob<'r>>` for every `'r`. `impl_cdupclone!` and
  `impl_cguarded!` name the lifetime, `<'r> BlobFree, Blob<'r>`, as
  [ownership.md](ownership.md#policies--what-teardown-means) explains.
- **A routine of another shape** (a slot, an out-parameter, extra arguments)
  is no reason to hand-write a policy. Pass the macro a private `unsafe fn`
  adapter, which gives the routine the shape that the macro calls, as
  [ownership.md](ownership.md#policies--what-teardown-means) describes.

| Item | Role | Source |
|------|------|--------|
| `CCell` | The linking trait: `type C` (the FFI type), `type Ref<'a>`, `type Mut<'a>`, and the provided, `unsafe` inbound seam (`ref_from_ptr`, `mut_from_ptr` and their `void *` twins). Its `unsafe impl` promises that `Self` is layout-compatible with `C`; that `Ref` / `Mut` are `#[repr(transparent)]` over `CBorrowedPtr<'a, Self>` with no `Drop`; and that `Mut` is invariant in `Self` (a `PhantomData<&'a mut Self>` field). ffibox builds handles from that layout, and checks their size at compile time. | `src/traits.rs` |
| `CRefSeam` / `CMutSeam` / `CLockedSeam` | The handles' seams. Every method is provided, from the layout that `CCell` (or `CGuarded`) guarantees. Implement them on hand-written handles with `type Cell = Foo;`, as the macros do. Otherwise the handles have no `as_ptr` / `as_mut_ptr`. | `src/traits.rs` |
| `CBorrowedPtr<'a, T>` | What every handle wraps: one pointer tagged with the borrow's lifetime, `Copy` and covariant like `&'a T`. | `src/refs.rs` |
| `CPlainElem` | The marker for plain-data elements: every bit pattern is a valid value and there is no C-side identity. It admits `&[T]` from a `CVec`, as a `CSliceElem`. (A wrapped C type implements `CCell` instead, so `&[Foo]` does not typecheck.) It also makes copies that are read out of memory C keeps writing sound. It is not the same as initialized: an unfilled buffer is `CVec<MaybeUninit<T>, _>` until `assume_init`. | `src/traits.rs` |
| `CSliceElem` | The elements that a `CVec` hands out as `&[T]` / `&mut [T]`: every `CPlainElem`, and the owners, whose references cover a Rust value, never C memory. The owners are `CBox`, `CArc` and `CStrBox`, bare or in an `Option`, and `CVal`. A hand-written owner implements it on the same terms. | `src/traits.rs` |

**Layout.** `CBox`, `CArc` and `CStrBox` are `#[repr(transparent)]` over their
pointer, and their policy is stateless, held in no bytes. So
`Option<CBox<Foo, P>>` is a `ffi::foo *`, NULL for `None`, as Rust guarantees
for `Option<NonNull<_>>`. It therefore takes a raw pointer's place in a
`#[repr(C)]` struct, in a `foo **` buffer and in an `extern "C"` signature
alike.

A policy with state fails to build in those three owners. `CBoxState<Foo, P>`
holds it instead: it is `#[repr(C)]` over `{ptr, policy}`, and crosses as a
raw pointer.

`CVal<Foo, P>` is the size of `Foo` under a stateless policy. A buffer is
pointer + length, with the pointer NULL when the buffer is empty, as C's
`{ T *ptr; size_t len; }`.

## An array freed by one routine

A `CVec` releases an array of owners element by element. It then frees the
buffer through `CLenDrop`, which frees the buffer only (see
[ownership.md](ownership.md#choosing-an-owner-what-shape-is-the-owned-thing)).
That covers a C routine whose body is only those frees. A routine that does
more (takes a lock, updates a count, or releases an element other than by
freeing it) is not a policy. For such a routine, the wrapper writes the owner
itself, as `CVec` is written:

- Its fields are private: the pointer and the length. An `unsafe` constructor
  adopts them, and its contract is the routine's: the array is one that the
  routine frees, and its non-null elements are distinct allocations it owns.
- Its `Drop` calls the routine once, with both fields.
- It hands the elements out as borrows tied to `&self`: `FooRef<'_>` through
  `CRefSeam::from_ptr`, `&CStr`, or a `CSlice` of the pointers, which copies
  them out. It never hands them out as `&mut` to a slot. Through a `&mut` to
  a slot, safe code would store a pointer, or NULL, that the routine then
  frees.
