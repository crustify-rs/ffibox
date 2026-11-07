# Outbound

Outbound is Rust's side of the boundary: memory Rust owns that C holds, calls
or reads. The inbound documents, [ownership.md](ownership.md) and
[borrows.md](borrows.md), cover memory C owns held from Rust; this one walks
the other way, with the same terms.

ffibox is needed only where C **aliases** a value: keeps a pointer to it
after a call returns, frees it, or calls back into it. A pointer passed down
for one call and not kept is plain Rust: `&mut x as *mut _` for C that writes,
`&x as *const _` for C that only reads, a `&CStr` or a slice's `as_ptr()`.

## The primitives

| C does | Primitive | Rust counterpart |
|--------|-----------|------------------|
| frees an object or buffer Rust allocated | an owner under `RustAlloc`, surrendered with `into_raw` / `into_c` | `Box::into_raw` |
| frees a string or buffer with its own allocator | `CStrBox::from_c_str`, `CVec::from_slice`, `CVec::from_fn`, `CVec::new_uninit` under that allocator's policy | `CString`, `Vec` |
| calls a closure during one call | `CClosure`: `ctx()` and `fn_ctx_first()` / `fn_ctx_last()` | `&mut F` |
| keeps a closure or value, and releases it later | `CPayload`: `into_raw()`, `release_fn()`, `fn_ctx_first()` / `fn_ctx_last()` | `Box<F>` |
| returns an object through an out-parameter | `CBox::from_c_out`, `CStrBox::from_raw_out` | a return value |
| calls a Rust routine that must not unwind into it | `callback::barrier` | `catch_unwind` |
| frees, copies or constructs a ported type's objects | `export_cdrop!`, `export_cdupclone!`, `export_cnew!` | the C routines they replace |

## Choosing a primitive

- **C frees it.** The memory comes from the allocator C frees with. If that
  is C's own (`free`, `lib_free`), build it through that allocator's policy:
  `CVec::from_fn` and `CVec::new_uninit` with a `CLenAlloc` policy,
  `CVec::from_slice` and `CStrBox::from_c_str` with a copying one. If C frees
  it through a Rust routine, allocate with `RustAlloc` and offer the release
  with `export_cdrop!`. Then surrender it with `into_raw`.
- **C calls back during the call** (`foreach`, `qsort_r`). `CClosure::new`
  over `&mut F`. The trampoline's type is inferred from the C declaration it
  is passed to, and the closure's arguments are the C types, which the
  binding converts to handles before calling its consumer's closure. With
  `std`, a panic stops the C routine through the failure value and resumes
  after it, at `finish()`; a re-entrant invocation is refused the same way.
  `F: Send` unless the routine calls back on the calling thread only
  (`unsafe CClosure::new_local`).
- **C keeps the callback** (`set_log_callback(cb, ctx, free)`). `CPayload`
  over an `Fn` closure, surrendered with `into_raw`, released by C through
  `release_fn()`. C may call it from several threads or re-entrantly, so it
  is reached as `&F`, and `into_raw` requires `Send + Sync`; state it changes
  goes behind a `Mutex` or an atomic. A panic in it aborts, so a fallible
  body goes in `barrier`.
- **C keeps a value** (a container's payload, user data). `CPayload<T>`, and
  `CPayload::borrow` inside the callbacks that receive it.
- **C returns something through an out-parameter.** `from_c_out` /
  `from_raw_out`: the slot starts NULL and is adopted whatever the routine
  returns, so an object handed over on failure is still released.
- **C still frees a type Rust now allocates** (a port in progress).
  `export_cnew!` / `export_cdrop!` / `export_cdupclone!` with the type's
  policy, in the module's `ffi_export` gateway, so that C allocates and frees
  through Rust's allocator, never `malloc` / `free` crossed with it.

## Conventions

- **Surrendering is safe, adopting is `unsafe`**, inbound and outbound alike:
  `into_raw` gives a pointer away, `from_raw` asserts what it addresses.
- **What one allocator allocates, only it frees.** The policy in the owner's
  type enforces it on the Rust side; an export, by naming the policy, on C's.
- **No unwinding into C.** A panic that reaches an `extern "C"` function
  aborts; `barrier`, and `CClosure` with `std`, turn it into a value first.
- **Stable addresses.** A value C keeps a pointer to lives behind a pointer
  that does not move while C holds it: a `CBox`, a `CPayload`, a `CClosure`
  borrowed for the call. A by-value `Foo` or `CVal` moves, so C must not keep
  a pointer into one.
