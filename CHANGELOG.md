# Changelog

What changes between releases of ffibox, and what an upgrade asks of code
written against the release before. A change that compiles unchanged but
behaves differently, or that adds an obligation to an `unsafe impl`, has a
migration note here: nothing else reports it.

## Unreleased

Changes on `devel` since 0.1.1.

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

- **A policy with state no longer fits `CBox`, `CArc` or `CStrBox`**
  (`2398db0`), which are now `#[repr(transparent)]` over their pointer. Such
  a policy fails at monomorphization, so `cargo build` reports it where
  `cargo check` does not. Hold the object in a `CBoxState`, adopted with
  `from_raw_with`; a counted object or a string under such a policy has no
  stateful owner yet.
