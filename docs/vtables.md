# C tables implemented in Rust

A C library often takes a **table**: a struct of function pointers that the
library calls into. C passes each **slot**, one function pointer of the
table, a pointer to the table itself. The implementer embeds the table at the
start of a larger allocation that also holds the implementer's own state.
libgit2's `git_config_backend`, `git_odb_backend` and `git_writestream` are
such tables; so is any "C inheritance" base struct with a `free` slot.

In Rust, you implement a table with a trait, and ffibox supplies the rest:

- [`CTable<T, S>`](#a-a-generic-owner-and-a-macro-for-the-slots-only) holds
  Rust state `S` behind the C table of the layout type `T`.
- [`impl_cvtable!`](#declaring-a-table) mints the slots.

A `CTable` is `Box<dyn Trait>` handed to C: a data part and a table of
functions over it, with the table embedded rather than pointed to.

The inbound counterpart is a table that C implements and Rust holds. It needs
nothing new: it is a `CBox` under a policy whose release calls the table's
`free` slot (`impl_cdrop!` over a private adapter), and you call its slots
through its handles. A binding (your Rust wrapper over the C library) may
have one owner for both kinds of table, as libgit2's write streams have. Such
a binding adopts a surrendered `CTable` into that owner (see
[(b)](#b-slots-that-hand-out-c-memory-and-the-tables-own-owner)).

You still write what is specific to the table: each slot's body. The body
turns C's arguments into Rust's, calls the trait, and turns the result into
C's status convention. ffibox supplies only what is the same for every table:

- the layout;
- the recovery of the state from the table pointer;
- the re-entry rule;
- the panic barrier;
- the release; and
- the owner.

## Decisions

### (a) A generic owner, and a macro for the slots only

`CTable<T, S>` is ordinary generic code. Its layout, the entry protocol, the
release and the owner's methods are written once, checked by the compiler and
run under Miri. The layout is `#[repr(C)]`: the C table first, then the entry
state (see [(c)](#c-receivers-and-re-entry)), then `S`. You write no struct
of your own, so the table cannot be placed anywhere but at offset zero; a
compile-time assertion keeps it there.

What generics cannot express is the slots. A slot is an `extern "C" fn` of
the C declaration's exact signature, with the table at whichever argument C
passes it in. `impl_cvtable!` mints the slots, and nothing else. It emits:

- one trampoline per slot, whose whole body is a call to `CTable::enter`; and
- the `CSlots<S>` impl that installs the trampolines. `CSlots` is a safe
  trait with two items, `base` and `install`.

The trampolines are local to `install`. `install` takes a token that only
`CTable` makes. The token is branded with that one borrow of the table and
with the impl's types. So a slot cannot be named, let alone installed in
another table or over another state type: each slot reaches `S` only through
the table it was installed in. That removes the recovery `unsafe` from every
slot.

What types cannot check is stated by one `unsafe impl CVtable<S> for T {}`
per table. `CVtable` is a trait with no items, and the `unsafe impl` states
its contract:

- C passes each slot its table in the argument marked `@`. Types check this
  where the table's argument is the only argument of its type. They cannot
  check it where C passes another table of that type too, as in
  `cmp(ops *self, ops *other)`.
- No slot that C calls is left NULL.

You write that line out in the macro's input. The macro emits it with your
own `unsafe` and braces, so your binding's lints see it:
`#![forbid(unsafe_code)]` rejects the invocation, and clippy's
`undocumented_unsafe_blocks` asks for its `SAFETY` comment. The rest of C's
contract with the table is stated where the table is handed to C.

The table starts from `CSlots::base`, which you write out as `base = …;`.
There is no default, because `Default::default()`, which leaves every slot
NULL, is right only when every slot that C calls unconditionally is declared.
`install` then sets the slots that it implements. Safe code therefore chooses
no slot: when a safe handle method of your binding calls a slot of the table,
it never runs a function that a consumer put there. If a table has fields
that are set per instance, or a C routine initialises it, build it with the
`unsafe` `CTable::with_base`. Its caller states the same of the base it
passes.

The alternative is also supported: a generic `CTable<T: Table>` with an
`unsafe` trait per C table and hand-written trampolines. It is supported
because `enter` and `release` are public. A slot that the macro cannot
express, such as a release of another shape, is written by hand on the same
terms, with one `unsafe` block per slot. A slot takes doc
comments only: an attribute such as `#[target_feature]` would add an
obligation to calling the slot that C cannot see. The macro is preferred
because it ties every slot to its table and state by construction. It is kept
to what an `unsafe impl` written by hand would contain.

### (b) Slots that hand out C memory, and the table's own owner

A slot may hand C an object: an entry, an iterator, a buffer. The slot
returns the object through the owner that the object already has: the body
builds the owner and surrenders it into C's out-parameter with `into_c` /
`into_raw`. A nested table (an iterator, an entry with its own `free`) is
another `CTable`, surrendered the same way and released by its own `free`
slot. Nothing new is needed: surrendering is safe, and writing C's
out-parameter is your binding's `unsafe`, as it is today.

The table itself is owned by `CTable<T, S>` while Rust holds it. `CTable`
lends the table's handles, `T::Ref` / `T::Mut`, as `CBox` does. So your
binding's methods on the handles (a `write` that calls the slot) work on a
Rust table as on a C one.

Those handles are your binding's: they are `Send` or `Sync` as you decided
for the C type, not for `S`. Their methods may call any slot, so `as_ref` and
`as_mut` ask `S: Send + Sync`, as a `&RwLock<S>` shared between threads does.

Where the handles stay on their thread, as `define_ctype!`'s do unless you
opt in, say so once per type with `impl_clocal_handles!(T)`. The macro checks
at compile time that neither handle is `Send` or `Sync` before it implements
`CLocalHandles`. `as_ref_local` / `as_mut_local` then lend the handles,
safely, over any `S`.

`into_c` surrenders the table to C, which releases it through the slot
declared `release`. C keeps the table past any borrow and may use it on any
thread, so `into_c` asks `S: Send + Sync + 'static`, as `CPayload::into_raw`
asks `Send + Sync`. The `unsafe` `into_c_local` asks only `'static`; its
caller states the threads C uses.

Your binding may have one owner for every table of that type, C's and Rust's
alike: for example `CBox<WriteStream, WriteStreamFree>`, whose policy calls
the `free` slot. In that case, adopt the surrendered pointer into that owner
with `from_c`. `from_c` is `unsafe`, as every adoption is; its obligation
here is that the policy releases through the `free` slot. ffibox does not add
a trait for that obligation, which is one block per table type.

### (c) Receivers and re-entry

A slot declares its receiver in its body's argument type: `|s: &S|` is a
**shared** slot, and `|s: &mut S|` is an **exclusive** one. `enter` infers
the receiver from the closure, so a slot cannot get a receiver that it did
not declare. The entry state is a non-blocking read-write lock over `S`, as
`RwLock::try_read` / `try_write` are:

- Any number of shared slots run at once, re-entrant or on several threads.
- An exclusive slot runs only when no other slot does.
- A slot that cannot enter is **refused**: it returns the value it declares
  `on busy`, without reaching `S`. An exclusive slot cannot enter while
  another slot runs, and a shared slot cannot enter while an exclusive one
  runs.

So re-entry is declared per slot, by its receiver. A table whose slots all
take `&S` is refused in two cases only: while a release of the table is
deferred, as described below, and when the count of shared slots running at once is full
(2^(`usize::BITS` − 4) − 1 of them). The owner's own accessors take part:

- `state(&self)` returns `Option<&S>`: `None` while an exclusive slot runs,
  and `Some(&S)` otherwise. Once it has lent `&S`, exclusive slots are
  refused until the owner is next borrowed mutably, through `state_mut`,
  `as_mut` or `as_mut_local`. The owner cannot see when
  the `&S` ends, so it waits for the `&mut self` that proves the `&S` has
  ended.
- `state_mut` and `as_mut` take `&mut self`.

Release follows the same state:

- If C calls `free` while a slot runs, whether re-entrantly from inside the
  slot or on another thread, the release is deferred. The slot that exits
  last frees the allocation, on its own thread, and every slot entered while
  the drop is deferred is refused.
- If C calls `free` on a table that the owner still holds, the release is
  deferred to the owner's drop, and every slot is refused while it is
  deferred. If the owner instead surrenders the table to C, the release is
  forgotten: C holds the table anew, and its slots enter again.

Each allocation is dropped exactly once, whichever order C and Rust release
it in.

### (d) Panics

Every trampoline runs its body in `callback::barrier`. A slot declares what a
panic returns `on panic`, as it declares `on busy`: an error code, a NULL, or
`std::process::abort()` for a slot whose C convention cannot fail. There is
no default, so no two bindings of one convention disagree by omission.
Without `std` there is no barrier, and a panic aborts at the `extern "C"`
boundary. The `release` slot has no result: a panic in `S`'s drop there
aborts, as `CPayload::release_fn`'s does. A panic does not poison a slot:
later calls run, and `S` is left as the unwinding left it, as after any
`barrier`.

## Declaring a table

```rust,ignore
ffibox::impl_cvtable! {
    // SAFETY: libgit2 passes a stream's members the stream, first, and calls
    // `write`, `close` and `free` on every stream.
    unsafe impl<W: StreamWriter> CVtable<W> for WriteStream {}
    base = ffi::git_writestream::default();
    /// `git_writestream.write` over `StreamWriter::write`.
    fn write(@stream, buffer: *const c_char, len: usize) -> c_int {
        on panic: std::process::abort(),
        on busy: busy().raise(),
        |w: &mut W| returned(w.write(unsafe { bytes(buffer, len) })),
    }
    fn close(@stream) -> c_int {
        on panic: std::process::abort(),
        on busy: busy().raise(),
        |w: &mut W| returned(w.close()),
    }
    release free;
}

let table = CTable::<WriteStream, W>::new(writer);
```

- `unsafe impl<…> CVtable<S> for T {}` names the layout type `T` and the
  state `S`. `T` is a `CCell`, such as one from `define_ctype!`, named by a
  path, with type arguments if it has any. Each generic takes one trait-path
  bound. The macro emits the line as written, so its `SAFETY` comment is
  yours. The comment states which argument C passes each slot its table in,
  and that no slot C calls is left NULL.
- `base = expr;` is the table before the slots are set, written out.
- `fn slot(…)` names the C field, then its C arguments in order. The table's
  argument is marked `@name`; `name` is the table pointer inside the body.
  Only doc comments go before the `fn`.
- `on panic:` and `on busy:` are the slot's results when its body panics or
  when it is refused. Both are expressions, evaluated only then.
- The body is a closure over the receiver. Its other inputs are the C
  arguments, captured by name.
- `release field;` installs `CTable::release` in the slot that `field` names
  (`free` above). Write it if C frees the table.

## Not a table: hooks without a context

`git_allocator`, and reftable's `reftable_set_alloc(malloc, realloc, free)`,
are families of hooks that C calls with no pointer back to anything. There is
no table to recover state from, nothing to release and no owner. They stay
trampolines generic over the implementing type. Each body runs in
`callback::barrier` with its declared result, and C's `free` hook, which
cannot fail, aborts on a panic. That is the rule both allocator bindings
follow; there is no primitive to add.

## What the binding still states

| Obligation | Where |
|------------|-------|
| C passes each slot its table in the argument marked `@`; `base` leaves NULL only slots C lets be NULL | the `unsafe impl CVtable` in `impl_cvtable!`'s input |
| C calls each slot only with the table it read it from, while the table is live, and calls `release` once | the `unsafe` C call that lends (`as_c_ptr`) or surrenders (`into_c`) the table |
| C uses the table on a thread other than `S`'s only if `S` allows it. Shared slots need `S: Send + Sync`: they share `&S`, and may drop `S` there as the last slot out of a deferred release. Exclusive slots and the release need `S: Send` | `into_c` asks `S: Send + Sync`. Otherwise: `into_c_local`, and the C call that receives a table lent through `as_c_ptr` or a handle method. `CTable` itself is `Send` / `Sync` on `RwLock<S>`'s terms, and `T`'s |
| a table built from a base of the caller's chooses no slot `install` leaves | `CTable::with_base` |
| nothing derived from `T`'s handles that keeps the table pointer is `Send` or `Sync` (the handles themselves are checked by `impl_clocal_handles!`) | `impl_clocal_handles!(T)`, for `as_ref_local` / `as_mut_local` |
| the policy of the owner a surrendered table is adopted into releases it through its `release` slot | that `from_c` |
| no safe setter of `T`'s handles replaces a slot ffibox installed | `T`'s handles |
| converting C's arguments, writing its out-parameters | each slot's body |
