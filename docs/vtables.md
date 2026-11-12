# C tables implemented in Rust

A C library often takes a **table**: a struct of function pointers it calls
into, each slot passed a pointer to the table itself, which the implementer
embeds at the start of a larger allocation holding its own state. libgit2's
`git_config_backend`, `git_odb_backend` and `git_writestream` are such tables;
so is any "C inheritance" base struct with a `free` slot. Rust implements one
with a trait, and ffibox supplies the rest: [`CTable<T, S>`](#the-primitive)
holds Rust state `S` behind the C table of the layout type `T`, and
[`impl_cvtable!`](#declaring-a-table) mints the slots. It is `Box<dyn Trait>`
handed to C: a data part and a table of functions over it, the table embedded
rather than pointed to.

The inbound counterpart, a table C implements held from Rust, needs nothing
new: it is a `CBox` under a policy whose release calls the table's `free`
slot (`impl_cdrop!` over a private adapter), and its slots are called
through its handles. A binding with one owner for both, as libgit2's write
streams have, adopts a surrendered `CTable` into it (see (b)).

The binding still writes what is specific to the table: each slot's body,
which turns C's arguments into Rust's, calls the trait, and turns the result
into C's status convention. ffibox supplies only what is the same for every
table: the layout, the recovery of the state from the table pointer, the
re-entry rule, the panic barrier, the release and the owner.

## Decisions

### (a) A generic owner, and a macro for the slots only

`CTable<T, S>` is ordinary generic code: its layout (`#[repr(C)]`, the C table
first, then the entry state, then `S`), the entry protocol, the release, and
the owner's methods are written once, checked by the compiler and run under
Miri. The binding writes no struct of its own, so the table cannot be placed
anywhere but at offset zero; a compile-time assertion keeps it there.

What generics cannot express is the slots: an `extern "C" fn` of the C
declaration's exact signature, with the table at whichever argument C passes
it. `impl_cvtable!` mints those, and nothing else: one trampoline per slot,
whose whole body is a call to `CTable::enter`, and the `CVtable<S>` impl that
installs them. The trampolines are local to the impl, and `install` takes a
token only `CTable` makes, so a slot cannot be named, let alone installed in
another table or over another state type: each slot reaches `S` only through
the table it was installed in. That removes the recovery `unsafe` from every
slot. What types cannot check is left as one `unsafe impl` per table, written
in the macro's input with its `SAFETY` comment: that C passes each slot its
table in the argument marked `@`. Types check that where the table's is the
only argument of its type, but not where C passes another table of that type
too (`cmp(ops *self, ops *other)`). The rest of C's contract with the table is
stated where the table is handed to C.

The table starts from `CVtable::base`, which the impl gives (`base = …;`,
by default `Default::default()`, every slot NULL), and `install` sets the
slots it implements. Safe code therefore chooses no slot: a binding's safe
handle method that calls a slot of the table never runs a function a consumer
put there. A table whose fields are set per instance, or that a C routine
initialises, is built with the `unsafe` `CTable::with_base`, whose caller
states the same of its base.

The alternative, a generic `CTable<T: Table>` with an `unsafe` trait per C
table and hand-written trampolines, is also supported, since `enter` and
`release` are public: a slot the macro cannot express (a release of another
shape) is written by hand on the same terms, with one `unsafe` block per
slot. A slot takes doc comments only: an attribute such as
`#[target_feature]` would add an obligation to calling it that C cannot see. The macro is preferred
because it ties every slot to its table and state by construction; it is kept
to what an `unsafe impl` written by hand would contain (G20).

### (b) Slots that hand out C memory, and the table's own owner

A slot that hands C an object, an entry, an iterator, a buffer, returns it
through the owner that object already has: the body builds the owner and
surrenders it into C's out-parameter with `into_c` / `into_raw`. A nested
table (an iterator, an entry with its own `free`) is another `CTable`,
surrendered the same way and released by its own `free` slot. Nothing new is
needed: surrendering is safe, writing C's out-parameter is the binding's
`unsafe`, as it is today.

The table itself is owned by `CTable<T, S>` while Rust holds it. It lends
the table's handles, `T::Ref` / `T::Mut`, as `CBox` does, so the binding's
methods on them (a `write` that calls the slot) work on a Rust table as on a
C one. Those handles are the binding's, `Send` or `Sync` as the binding
decided for the C type, not for `S`; since their methods may call any slot,
`as_ref` and `as_mut` ask `S: Send + Sync`, as a `&RwLock<S>` shared between
threads does, and `as_ref_local` / `as_mut_local` lend them for any `S` to a
caller that keeps them on its thread, as `define_ctype!`'s handles are unless
the binding opts in. `into_c` surrenders the table to C, which releases it
through the slot declared `release`; C keeps it past any borrow, so it takes
`S: 'static`. Where the binding has one owner for every table of that
type, C's and Rust's alike (`CBox<WriteStream, WriteStreamFree>`, whose
policy calls the `free` slot), the surrendered pointer is adopted into it with
`from_c`, `unsafe` as every adoption is: its obligation is that the policy
releases through the `free` slot. ffibox does not add a trait for that
obligation, which is one block per table type.

### (c) Receivers and re-entry

A slot declares its receiver in its body's argument type: `|s: &S|` is a
**shared** slot, `|s: &mut S|` an **exclusive** one, inferred by `enter`
from the closure, so a slot cannot get a receiver it did not declare. The
entry state is a non-blocking read-write lock over `S`, as
`RwLock::try_read` / `try_write` are:

- any number of shared slots run at once, re-entrant or on several threads;
- an exclusive slot runs only when no other slot does;
- a slot that cannot enter, an exclusive one while another slot runs or a
  shared one while an exclusive one does, is **refused**: it returns the value
  it declares `on busy`, without reaching `S`.

So re-entry is declared per slot, by its receiver, and a table whose slots
all take `&S` is never refused. The owner's own accessors take part:
`state(&self)` lends `&S` and refuses exclusive slots until the owner is next
borrowed mutably (it cannot see when the `&S` ends, so it waits for the
`&mut self` that proves it has); `state_mut` and `as_mut` take `&mut self`.

Release follows the same state. C's `free` while a slot runs, re-entrantly
from inside it or on another thread, is deferred: the slot that exits last
frees the allocation, on its own thread, and every slot entered while the
drop is deferred is refused. A `free` C calls on a table the owner still
holds is deferred to the owner's drop, and forgotten if the owner then
surrenders the table to C, which holds it anew. Each allocation is dropped
exactly once, whichever order C and Rust release it in.

### (d) Panics

Every trampoline runs its body in `callback::barrier`, and a slot declares
what a panic returns `on panic`, as it declares `on busy`: an error code, a
NULL, or `std::process::abort()` for a slot whose C convention cannot fail.
There is no default, so no two bindings of one convention disagree by
omission. Without `std` there is no barrier, and a panic aborts at the
`extern "C"` boundary. The `release` slot has no result: a panic in `S`'s
drop there aborts, as `CPayload::release_fn`'s does. A slot is not poisoned
by a panic: later calls run, and `S` is left as the unwinding left it, as
after any `barrier`.

## Declaring a table

```rust,ignore
ffibox::impl_cvtable! {
    // SAFETY: libgit2 passes a stream's members the stream, first.
    unsafe impl<W: StreamWriter> CVtable<W> for WriteStream {
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
}

let table = CTable::<WriteStream, W>::new(writer);
```

- `unsafe impl<…> CVtable<S> for T` names the layout type `T` (a `CCell`,
  from `define_ctype!`) and the state `S`; each generic takes one trait-path
  bound. Its `SAFETY` comment states which argument C passes each slot its
  table in.
- `base = expr;`, optional, is the table before the slots are set; the
  default is `Default::default()`.
- `fn slot(…)` names the C field, then its C arguments in order, the table's
  marked `@name`; `name` is the table pointer inside the body. Only doc
  comments go before it.
- `on panic:` and `on busy:` are the slot's results when its body panics or
  it is refused; both are expressions, evaluated only then.
- The body is a closure over the receiver; its other inputs are the C
  arguments, captured by name.
- `release field;` installs `CTable::release` in the `free` slot, if C frees
  the table.

## Not a table: hooks without a context

`git_allocator`, and reftable's `reftable_set_alloc(malloc, realloc, free)`,
are families of hooks that C calls with no pointer back to anything: there is
no table to recover state from, nothing to release and no owner. They stay
trampolines generic over the implementing type, each body in
`callback::barrier` with its declared result, and C's `free` hook, which
cannot fail, aborting on a panic. That is the rule both allocator bindings
follow; there is no primitive to add.

## What the binding still states

| Obligation | Where |
|------------|-------|
| C passes each slot its table in the argument marked `@` | the `unsafe impl` in `impl_cvtable!`'s input |
| C calls each slot only with the table it read it from, while the table is live, and calls `release` once | the `unsafe` C call that lends (`as_c_ptr`) or surrenders (`into_c`) the table |
| C uses the table on a thread other than `S`'s only if `S` allows it: `S: Send + Sync` for shared slots, which share `&S` and may drop `S` there as the last slot out of a deferred release, and `S: Send` for exclusive slots and the release | the same call; `CTable` itself is `Send` / `Sync` on `RwLock<S>`'s terms, and `T`'s |
| a table built from a base of the caller's chooses no slot `install` leaves | `CTable::with_base` |
| `T`'s handles stay on their thread | `as_ref_local` / `as_mut_local` |
| the policy of the owner a surrendered table is adopted into releases it through its `release` slot | that `from_c` |
| no safe setter of `T`'s handles replaces a slot ffibox installed | `T`'s handles |
| converting C's arguments, writing its out-parameters | each slot's body |
