# Goals

ffibox is one vocabulary for every value whose memory, layout or lifetime
Rust shares with a foreign language, in both directions:

- **inbound**: memory the foreign side allocates, frees, copies, refcounts or
  locks, held from Rust;
- **outbound**: memory Rust owns that the foreign side holds, calls, reads or
  writes, including Rust objects whose layout it knows and Rust statics it
  names;
- **the call boundary**: the arguments and results of a foreign function Rust
  calls, and of a Rust function the foreign side calls (an export).

It serves bindings, safe Rust over foreign code that stays, and ports, Rust
that replaces foreign code and keeps its ABI for the code that remains and
for its consumers. The foreign side meets Rust at the C ABI, as G8 states.

Two parties use each primitive:

- a **producer** implements its unsafe internals: a policy, a layout type, a
  seam, an export. A producer may be a human, an agent or a static analysis
  tool;
- a **consumer** uses it through its safe API, without knowing whether C or
  Rust sits behind it.

ffibox is needed where the foreign side **aliases** a value: keeps a pointer
to it beyond a call, frees it, shares its refcount or lock, or writes it while
Rust holds it. It is also needed where the foreign side knows a value's
layout, and at the call boundary, where a pointer becomes a reference.

A pointer lent for one call becomes a plain `&` or `&mut` only if nothing
else reaches its target while the reference lives: no other argument, no
callback, no other thread, and no free made by the call itself. Memory the
foreign side neither aliases nor lays out is plain Rust: `&mut`, `&`, `Box`,
`Vec`, `Arc`.

The goals below are grouped by what they serve. Each states what it means and
how it is checked.

## Familiarity

### G1. Mirror standard Rust

Each primitive mirrors a standard Rust one a Rust programmer already knows:
`CBox` is `Box`, `CArc` is `Arc`, `CVec` is `Vec`, `CGuard` is `MutexGuard`,
`FooRef` / `FooMut` are `&` / `&mut`.

Where a foreign convention has no standard counterpart, it is expressed as the
closest composition of standard idioms rather than as a new concept:

- a lock over part of an object is `Arc<T>` with a `Mutex` field;
- a refcount that proves sole ownership is `Arc::get_mut`.

Where ownership allows it, each primitive converts to and from its standard
counterpart (`Box`, `Vec`, `Arc`, `CStr`, `&[T]`) and implements the standard
traits its counterpart does (`Clone`, `Debug`, `Iterator`, `Eq`, `Hash`), so
that a consumer can leave ffibox as soon as the foreign side stops aliasing.

*Checked by:* every primitive names its standard counterpart in its
documentation, and every departure from it states why.

### G2. No more discipline than standard Rust

A consumer needs no more care with a primitive than with its standard
counterpart: no ordering rules, no manual release, no invariants to remember.
In particular, a consumer writes no `unsafe`.

*Checked by:* consumer code, and the tests of the safe interface, compile with
`#![forbid(unsafe_code)]`.

### G3. Continue ffibox's style

New primitives continue the style ffibox has established:

- its terms (owner, handle, policy, seam, adopt, surrender);
- its naming patterns (`C`-prefixed owners, `Foo` / `FooRef` / `FooMut` per
  type, `impl_*!` per policy trait);
- its preference for reusing one primitive across cases over adding one per
  case.

This is a direction, not a freeze. A new term, name or primitive is welcome
where it is clearer than stretching an existing one. It states what it adds
and how it relates to the terms beside it.

*Checked by:* the documentation's walk from a foreign declaration to its
wrapper stays a short decision table per direction, and each term it uses is
defined once.

## Expressiveness

### G4. Ownership conventions as Rust idioms

A foreign language's ownership conventions are expressed with Rust's own:

- `Drop` for release;
- `Clone` for copies and reference counts;
- lifetimes for borrows and keepalive;
- `&` / `&mut` exclusivity for access;
- `Send` / `Sync` for thread affinity;
- `Pin` for address sensitivity;
- `Option` for nullability;
- slices for cardinality.

A convention that one primitive cannot express is a gap in the vocabulary, not
a reason for a raw pointer in the consumer's API.

*Checked by:* the public interface of a binding carries no raw pointer and no
`unsafe fn` except where an obligation cannot be expressed in types, each
such case documented.

### G5. Foreign patterns as Rust patterns

Beyond ownership, a foreign API follows recurring patterns, and each is
expressed as the standard Rust trait or idiom it corresponds to, declared in
the foreign language's own terms: the producer names the foreign routines and
conventions, and the primitive supplies the Rust side.

| foreign pattern | Rust pattern |
|-----------------|--------------|
| a cursor advanced by `next(it)` until NULL or a status; a `count` and `get(i)` pair; a `->next` chain; a sentinel-terminated array | `Iterator`, `IntoIterator`, slices |
| a `foreach(callback, context)` visitor | a closure, or an `Iterator` where the visit can be suspended |
| a status code, `errno`, an error out-parameter | `Result` with a typed error |
| out-parameters | return values |
| a read or write callback pair over a stream | `Read`, `Write`, `Seek` |
| a key-value option setter, a configuration struct filled before use | a builder |
| a struct of function pointers a library calls into | a trait the Rust side implements |

A pattern is one primitive, parameterised by the routines that implement it,
so that every API using it reads the same in Rust. In the outbound direction,
the same primitive offers a Rust iterator, error or trait to the foreign side
in its own convention.

*Checked by:* the public interface of a binding exposes no foreign pattern
whose Rust counterpart exists: no cursor to advance by hand, no status code to
check, no out-parameter to fill.

### G6. Both directions, symmetrically

Every primitive serves both directions with the same terms:

- adopting has surrendering;
- a policy that binds a foreign routine has an export that offers a Rust
  routine to the foreign side;
- a foreign callback has a Rust closure behind a foreign function pointer;
- a foreign global has a Rust static the foreign side reads.

Either side can produce any ownership shape, and either can consume it.

*Checked by:* every inbound primitive names its outbound counterpart, or
states why it has none.

### G7. Typed exports

A Rust function the foreign side calls, an export, receives its arguments as
types with the pointer's ABI that state what the caller guarantees:

- a string;
- an out-parameter to write;
- an object borrowed for the call;
- a run of elements to read, or to fill from uninitialised memory;
- a callback with its context.

Its body converts no pointer by hand. A panic becomes the routine's own
failure value (G11). An export that replaces a foreign routine makes the
checks the routine made, in the same order, so that the same invalid argument
yields the same result.

*Checked by:* an export's body has no `unsafe` for its arguments, and each
argument type has a compile-fail test for the misuse it rules out.

### G8. The C ABI

ffibox serves the C ABI and the conventions of APIs at that ABI: allocator
pairs, refcounts, locks, NUL-terminated strings, status codes and `errno`,
callbacks with a context pointer. Any language that exports or imports
through the C ABI is served by them, and ffibox's names are the ABI's:
`C`-prefixed owners, `c_drop`, `as_c_ptr`.

Conventions of one language behind the ABI, such as C++ exceptions or Go's
pointer-passing rules, are out of scope until a binding needs them. They are
then added beside these, not folded into them.

*Checked by:* every primitive serves a convention an API at the C ABI uses.

## Safety

### G9. UB hazards excluded by construction

A binding built from the primitives cannot reach undefined behaviour from safe
code. Each hazard is excluded by a primitive or a producer's contract:

| hazard | excluded by |
|--------|-------------|
| aliasing memory the foreign side can reach | for a foreign-defined layout, handles: no Rust reference over memory that anything may write or free while the reference lives (the foreign side, another handle, another thread), and no `&mut` over memory anything else may access; for a Rust-defined type, `&T` only where every byte the foreign side or another thread writes while `&T` lives is an atomic or behind `UnsafeCell`, and nothing frees it while a reference lives, including a reference argument, which must stay valid until its function returns |
| moving an address-sensitive object | pinned owners; inline values only for movable layouts |
| null dereference | `NonNull`, `Option` with the null niche |
| invalid values (`_Bool`, `char`, out-of-range enums) | plain-element markers, open enums |
| reading uninitialised memory | `MaybeUninit`, construction phases, `assume_init` |
| misalignment | layout types, alignment bounds on copies |
| out-of-bounds access | slices and runs carrying their length |
| type confusion through erased pointers | typed erasure and recovery |
| use after free, outliving a keepalive | lifetimes, counted references |
| double release, release by the wrong routine | policies pairing release with allocation |
| data races | `Send` / `Sync` opt-in, lock scopes |
| re-entry through callbacks or exports | callback arguments typed by the access the caller holds; no reference held across a call that can reach the same object; where one must be, re-entry is refused or serialised |
| unwinding or `longjmp` across the boundary | panic barriers, documented non-local exits |

Leaks are not undefined behaviour and are not excluded, but no primitive leaks
except by an explicit surrender.

*Checked by:* each hazard has a test that the binding's safe interface cannot
reach it, a compile-fail test where the type system rejects it.

### G10. Safe by default

Every default is the conservative one:

- foreign types are neither `Send` nor `Sync`;
- adopting a pointer is `unsafe`;
- no status code is discarded;
- an unknown fact selects the more restrictive primitive, and establishing
  the fact later relaxes it.

Every relaxation is an explicit opt-in that states its justification.

*Checked by:* a binding with no opt-ins is sound, if less capable.

### G11. Failure semantics

- No panic crosses into the foreign language: exports and callbacks have a
  barrier that maps a panic to the routine's error convention.
- No foreign unwind or `longjmp` crosses Rust frames with destructors.
- Allocation failure is a value, `Option` or `Result`, never an abort.
- Teardown that fails or panics still releases each object exactly once.

*Checked by:* tests that panic in callbacks, exports and drops, under the
sanitizers.

### G12. Unsafe encapsulated, obligations local

`unsafe` stays with producers: in policy implementations, layout types, seams
and exports. It is crate-private to the binding wherever it can be.

Every unsafe trait and seam states a falsifiable `# Safety` contract in terms
of the foreign side's observable behaviour. The contract is checkable one
implementation at a time, without reasoning about the whole program. The
distinct obligations are few and listed, so that auditing a binding is a
checklist.

Each fact a binding relies on, such as ownership, nullability, keepalive or
thread affinity, is visible in its types, so that a reviewer or a tool
recovers the facts from the binding.

*Checked by:* every `unsafe impl` and `unsafe` block maps to one listed
obligation.

### G13. Type safety through traits

Generic code is bound by traits rather than raw pointers:

- a policy implements the lifecycle traits for the exact type it releases;
- a routine of the wrong type does not compile;
- erased values are recovered through the trait of the type they hold;
- a family of foreign types generated from one template or macro shares one
  generic wrapper.

*Checked by:* compile-fail tests binding a routine, an element or an erased
value to the wrong type.

## Cost

### G14. Zero overhead

A primitive costs what the equivalent raw code costs. With a stateless policy,
an owner is pointer-sized and a handle is one pointer. Runtime state is
carried only when the foreign contract needs it, and no check is made that the
foreign contract does not require.

*Checked by:* compile-time size assertions, and benchmarks against raw calls.

### G15. ABI fidelity

A primitive that stands in for a foreign value has a guaranteed ABI:

- `#[repr(transparent)]` where it crosses by value;
- the null pointer as `None`;
- the foreign layout where the foreign side reads or writes its fields.

*Checked by:* size, alignment and field offsets asserted against what the C
compiler computes for each supported target, never against Rust's own
numbers. The assertions cover shared prefixes, atomics and cells standing in
for foreign fields, bitfields and statics.

### G16. Stable Rust, `no_std`, no hidden allocation

ffibox builds on stable Rust, with a declared minimum version, and in
`no_std`. It allocates only through a policy or an allocator the consumer
chose. It uses the standard `Allocator` interface where a primitive needs a
non-global allocator. What only nightly Rust can express, such as Rust-defined
variadic functions, is documented as unsupported.

*Checked by:* CI on the minimum supported version, with and without `std`.

## Production

### G17. Patterns that recur across projects

A primitive captures an interop pattern that recurs across APIs, not one
project's convention. A pattern seen in one project stays in that project's
crate until another project needs it.

A combination of existing primitives is composed from them, not given a
primitive of its own: owners are elements of vectors, fields of `#[repr(C)]`
structs and contents of guards.

*Checked by:* each primitive names at least two independent APIs that use it.

### G18. Stable through migration

A consumer's code does not change when the producer behind a primitive
changes: a C allocator replaced by Rust's, a C implementation by a Rust
port. Only the policy changes. This is what lets a codebase be ported one
module at a time.

*Checked by:* a ported module's consumers compile unchanged against the port.

### G19. Simple macros

Macros mint only what traits and generics cannot: layout types, handles, and
the impls that name a foreign routine. Everything else is ordinary generic
code, provided methods and blanket impls, which the compiler checks and
documents.

Each macro does one thing, with a few arms that differ only in optional
trailing parameters. A variant whose expansion differs is a macro of its own
with its own name, as `impl_cdrop_str!` is beside `impl_cdrop!`, rather than
another arm of a larger one. A misuse fails with a type error at
the invocation, not inside the expansion.

*Checked by:* each macro's expansion reads as the impl a producer would write
by hand, and fits on a screen.

## Verification

### G20. A small trusted base

ffibox's own unsafe code is minimal and kept in few places, and each unsafe
block states which listed obligation it discharges (G12). ffibox's
guarantees are only as strong as this base.

*Checked by:* the lints that require safety comments on unsafe code are
enforced.

### G21. Every claim tested

Each claim ffibox makes has a test that fails if the claim is false:

- each misuse ffibox claims to prevent has a compile-fail test;
- the tests run under Miri with strict provenance, under both borrow models,
  with many seeds for concurrent primitives, and on big-endian and 32-bit
  targets;
- they run under the address, undefined-behaviour and thread sanitizers, and
  under BorrowSanitizer;
- each layout is checked against the C compiler for the target (G15).

A test compares Rust with the C compiler, the foreign routine or an
instrument, never with Rust's own numbers.

*Checked by:* the test matrix runs clean under each instrument before a
change lands.

## Non-goals

- **Finding the facts.** ffibox records ownership facts in types; it does not
  infer them. Inference is the producer's: a human, an agent or an analysis.
- **Generating bindings.** ffibox is what bindings are written in, not a
  generator. Its regularity serves generators.
- **Memory the foreign side neither aliases nor lays out.** That is standard
  Rust's.
