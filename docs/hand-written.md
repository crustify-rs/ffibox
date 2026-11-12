# Hand-written wrappers

Only a wrapper `define_ctype!` cannot express — generic parameters, a
lifetime-carrying layout type — needs this layer, and so does an owner no
policy expresses: [an array freed by one routine](#an-array-freed-by-one-routine)
that does more than free its elements and itself.

What is hand-written for such a wrapper is the layout type, its handles and
their seams, against the items below. Its policies are not, unless no macro
takes them:

- **A layout type with a lifetime** (`Blob<'r>`) takes `impl_cdrop!`,
  `impl_crefclone!` and `impl_cdispose!` as a `define_ctype!` type does, with
  the lifetime spelled `'_`: `impl_cdrop!(BlobFree, Blob<'_>, ffi::blob_free)`
  implements `CDrop<Blob<'r>>` for every `'r`. Only its `CDupClone` and
  `CGuarded` are hand-written, as
  [ownership.md](ownership.md#policies--what-teardown-means) explains, and
  [examples.md](examples.md#a-layout-type-with-a-lifetime--hand-written-ccell-macro-policies)
  shows the whole wrapper.
- **A layout type generic over a type** (`Stack<T, S>`) has no elided form,
  so each of its policies is an `unsafe impl<T, S>` by hand.
- **A routine of another shape** (a slot, an out-parameter, extra arguments)
  is no reason to hand-write a policy: a private `unsafe fn` adapter passed to
  the macro gives it the shape the macro calls, as
  [ownership.md](ownership.md#policies--what-teardown-means) describes.

| Item | Role | Source |
|------|------|--------|
| `CCell` | the linking trait: `type C` (the FFI type), `type Ref<'a>`, `type Mut<'a>`, and the provided, `unsafe` inbound seam (`ref_from_ptr`, `mut_from_ptr` and their `void *` twins). Its `unsafe impl` promises that `Self` is layout-compatible with `C`, and that `Ref` / `Mut` are `#[repr(transparent)]` over `CBorrowedPtr<'a, Self>` with no `Drop`, and `Mut` invariant in `Self` (a `PhantomData<&'a mut Self>` field) — ffibox builds handles from that layout, and checks their size at compile time. | `src/traits.rs` |
| `CRefSeam` / `CMutSeam` / `CLockedSeam` | the handles' seams, every method provided from the layout `CCell` (or `CGuarded`) guarantees: implement them on hand-written handles with `type Cell = Foo;`, as the macros do, or the handles have no `as_ptr` / `as_mut_ptr`. | `src/traits.rs` |
| `CBorrowedPtr<'a, T>` | what every handle wraps: one pointer tagged with the borrow's lifetime, `Copy` and covariant like `&'a T`. | `src/refs.rs` |
| `CPlainElem` | marker for plain-data elements: every bit pattern is a valid value and there is no C-side identity. It admits `&[T]` from a `CVec`, as a `CSliceElem` (a wrapped C type implements `CCell` instead, so `&[Foo]` does not typecheck), and makes copies read out of memory C keeps writing sound. Not the same as initialized: an unfilled buffer is `CVec<MaybeUninit<T>, _>` until `assume_init`. | `src/traits.rs` |
| `CSliceElem` | elements a `CVec` hands out as `&[T]` / `&mut [T]`: every `CPlainElem`, and the owners — `CBox`, `CArc` and `CStrBox`, bare or in an `Option`, and `CVal` — whose references cover a Rust value, never C memory. A hand-written owner implements it on the same terms. | `src/traits.rs` |

**Layout.** `CBox`, `CArc` and `CStrBox` are `#[repr(transparent)]` over their
pointer, and their policy is stateless, held in no bytes: `Option<CBox<Foo, P>>`
is a `ffi::foo *`, NULL for `None`, as Rust guarantees for `Option<NonNull<_>>`.
So it takes a raw pointer's place in a `#[repr(C)]` struct, a `foo **` buffer
and an `extern "C"` signature alike. A policy with state fails to build in
them; it is held by `CBoxState<Foo, P>`, `#[repr(C)]` over `{ptr, policy}`,
which crosses as a raw pointer. `CVal<Foo, P>` is the size of `Foo` under a
stateless policy; a buffer is pointer + length, the pointer NULL when empty,
as C's `{ T *ptr; size_t len; }`.

## An array freed by one routine

A `CVec` releases an array of owners element by element, then frees the
buffer through `CLenDrop`, which frees the buffer only (see
[ownership.md](ownership.md#choosing-an-owner-what-shape-is-the-owned-thing)).
That covers a C routine whose body is only those frees. One that does more
— takes a lock, updates a count, or releases an element other than by
freeing it — is not a policy: the wrapper writes the owner itself, as `CVec`
is written:

- private fields, the pointer and the length, adopted by an `unsafe`
  constructor whose contract is the routine's: the array is one the routine
  frees, and its non-null elements are distinct allocations it owns;
- a `Drop` that calls the routine once, with both;
- the elements handed out as borrows tied to `&self` — `FooRef<'_>` through
  `CRefSeam::from_ptr`, `&CStr`, a `CSlice` of the pointers, which copies
  them out — never as `&mut` to a slot, through which safe code would store
  a pointer, or NULL, that the routine then frees.
