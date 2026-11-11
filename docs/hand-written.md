# Hand-written wrappers

Only a wrapper `define_ctype!` cannot express — generic parameters, a
lifetime-carrying layout type — needs this layer.

| Item | Role | Source |
|------|------|--------|
| `CCell` | the linking trait: `type C` (the FFI type), `type Ref<'a>`, `type Mut<'a>`, and the provided, `unsafe` inbound seam (`ref_from_ptr`, `mut_from_ptr` and their `void *` twins). Its `unsafe impl` promises that `Self` is layout-compatible with `C`, and that `Ref` / `Mut` are `#[repr(transparent)]` over `CBorrowedPtr<'a, Self>` with no `Drop`, and `Mut` invariant in `Self` (a `PhantomData<&'a mut Self>` field) — ffibox builds handles from that layout, and checks their size at compile time. | `src/traits.rs` |
| `CRefSeam` / `CMutSeam` / `CLockedSeam` | the handles' seams, every method provided from the layout `CCell` (or `CGuarded`) guarantees: implement them on hand-written handles with `type Cell = Foo;`, as the macros do, or the handles have no `as_ptr` / `as_mut_ptr`. | `src/traits.rs` |
| `CBorrowedPtr<'a, T>` | what every handle wraps: one pointer tagged with the borrow's lifetime, `Copy` and covariant like `&'a T`. | `src/refs.rs` |
| `CPlainElem` | marker for plain-data elements: every bit pattern is a valid value and there is no C-side identity. It admits `&[T]` from a `CVec` (a wrapped C type implements `CCell` instead, so `&[Foo]` does not typecheck) and makes copies read out of memory C keeps writing sound. Not the same as initialized: an unfilled buffer is `CVec<MaybeUninit<T>, _>` until `assume_init`. | `src/traits.rs` |

**Layout.** `CBox`, `CArc` and `CStrBox` are `#[repr(transparent)]` over their
pointer, and their policy is stateless, held in no bytes: `Option<CBox<Foo, P>>`
is a `ffi::foo *`, NULL for `None`, as Rust guarantees for `Option<NonNull<_>>`.
So it takes a raw pointer's place in a `#[repr(C)]` struct, a `foo **` buffer
and an `extern "C"` signature alike. A policy with state fails to build in
them; it is held by `CBoxState<Foo, P>`, `#[repr(C)]` over `{ptr, policy}`,
which crosses as a raw pointer. `CVal<Foo, P>` is the size of `Foo`; a buffer
is pointer + length, the pointer NULL when empty, as C's
`{ T *ptr; size_t len; }`.
