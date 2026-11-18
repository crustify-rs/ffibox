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
for its consumers.

The foreign side meets Rust at the C ABI. ffibox serves the conventions of
APIs at that ABI: allocator pairs, refcounts, locks, NUL-terminated strings,
status codes and `errno`, callbacks with a context pointer. Conventions of
one language behind the ABI, such as C++ exceptions or Go's pointer-passing
rules, are out of scope until a binding needs them. They are then added
beside these, not folded into them.

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

The goals below are what a primitive meets before it is added to ffibox,
most important first. Each states a property and how it is checked. None
prescribes how a primitive meets it.

## Soundness

### G1. No undefined behaviour from safe code

Code that uses only a primitive's safe interface cannot reach undefined
behaviour, whatever the foreign side does within its documented contract.
The hazards are:

- aliasing memory the foreign side or another thread can reach, including
  through a reference argument, which must stay valid until its function
  returns;
- re-entry into an object through a callback or an export;
- data races;
- use after free, and outliving a keepalive;
- double release, and release by the wrong routine;
- null dereference;
- reading uninitialised memory;
- invalid values (`_Bool`, `char`, out-of-range enums);
- misalignment;
- out-of-bounds access;
- type confusion, through an erased pointer or a routine bound to the wrong
  type;
- moving an address-sensitive object;
- a panic, unwind or `longjmp` crossing the boundary.

A primitive states which hazards it excludes. Teardown that fails or panics
still releases each object exactly once. Leaks are not undefined behaviour,
but no primitive leaks except by an explicit surrender.

*Checked by:* for each hazard a primitive excludes, a test that its safe
interface cannot reach it, and a compile-fail test where the type system
rejects the misuse.

### G2. Obligations stated and local

`unsafe` stays with producers, and ffibox's own unsafe code is limited to what
safe Rust cannot express. Every unsafe trait, function and block states a
falsifiable obligation in terms of the foreign side's observable behaviour,
or discharges one. An obligation is checkable one implementation at a time,
without reasoning about the whole program. The distinct obligations are few
and listed, so that auditing a binding is a checklist.

An obligation does not rest on crate privacy: a private constructor is still
safe code. No macro hides an `unsafe` operation on an expression its caller
passes. Each fact a binding relies on, such as ownership, nullability,
keepalive or thread affinity, is visible in its types, so that a reviewer or
a tool recovers it from the binding.

*Checked by:* every `unsafe impl`, `unsafe fn` and `unsafe` block maps to one
listed obligation.

### G3. Conservative by default

Every default is the conservative one. An unknown fact selects the more
restrictive primitive, and establishing the fact later relaxes it. Every
relaxation is an explicit opt-in that states its justification.

*Checked by:* a binding with no opt-ins is sound, if less capable.

## Verification

### G4. Every claim tested against an oracle

Each claim a primitive makes, about the hazards it excludes, its layout, its
cost or its behaviour on failure, has a test that fails if the claim is
false. The test compares the primitive with an oracle independent of it: the
C compiler, the foreign routine, or a dynamic checker such as Miri or a
sanitizer. It never compares Rust with its own numbers. Claims that depend on
the target are tested on targets that differ from the host, such as
big-endian and 32-bit ones.

*Checked by:* each claim in a primitive's documentation names its test, and
the README's test matrix runs clean before a change lands.

## Fidelity to the foreign side

### G5. ABI fidelity

A primitive that stands in for a foreign value has the foreign value's ABI:
its size, alignment, field offsets and bit layout, how it is passed, and the
null pointer where the foreign side passes one.

*Checked by:* layouts asserted against what the C compiler computes for each
supported target.

### G6. Zero overhead

A primitive costs what the sound equivalent raw code costs. It carries
runtime state, and makes a check, only where the foreign contract requires
it, where the soundness of safe code requires it (as `Arc` bounds its count),
or where the project chose to give the foreign side's undefined behaviour a
defined result.

*Checked by:* compile-time size assertions, and benchmarks against raw calls.

### G7. Stable through migration

Replacing a foreign implementation with a Rust port changes neither side's
code:

- Rust consumers compile unchanged;
- the port keeps the foreign ABI, its headers, symbols and behaviour, so that
  the remaining foreign code and its consumers keep working.

This is what lets a codebase be ported one module at a time. Where a port
changes the primitive behind a value, the change is stated.

*Checked by:* a ported module's Rust consumers compile unchanged against the
port, and the foreign side's own tests pass against it.

## Idiomatic for consumers

### G8. Mirror standard Rust

Each primitive mirrors a standard Rust one that a Rust programmer already
knows, as `CBox` mirrors `Box`. A foreign convention is expressed through
Rust's own idioms for ownership, borrowing, iteration and errors, not as a
new concept. Where ownership allows it, a primitive converts to and from its
standard counterpart and implements the standard traits its counterpart does,
so that a consumer can leave ffibox as soon as the foreign side stops
aliasing.

*Checked by:* every primitive names its standard counterpart in its
documentation, and every departure from it states why.

### G9. No more discipline than standard Rust

A consumer needs no more care with a primitive than with its standard
counterpart: no ordering rules, no manual release, no invariants to remember,
no `unsafe`. This holds on both sides of a call. A binding's public interface
leaves no foreign pattern for the consumer to work by hand, such as a raw
pointer, a cursor to advance, a status code to check or an out-parameter to
fill. An export's body converts no pointer by hand. Where an obligation cannot
be expressed in types, the exception is documented.

*Checked by:* consumer code and the tests of the safe interface compile with
`#![forbid(unsafe_code)]`, and every raw pointer or `unsafe fn` in a public
interface is a documented exception.

### G10. Both directions

Every primitive serves both directions in the same terms: what Rust adopts
from the foreign side, Rust can surrender to it, and what Rust calls through
a primitive, Rust can offer through it.

*Checked by:* every inbound primitive names its outbound counterpart, or
states why it has none.

## Admission

### G11. Recurring, not composable

A primitive captures a pattern that recurs across APIs, not one project's
convention. A pattern seen in one project stays in that project's crate until
another project needs it. A combination of existing primitives is composed
from them, not given a primitive of its own. Stretching an existing primitive
is preferred to adding one, unless the new one is clearer. A new term is
defined once and states how it relates to the terms beside it.

*Checked by:* each primitive names at least two independent APIs that use it,
and each term is defined once.

### G12. Portable

ffibox builds on stable Rust, with a declared minimum version, and in
`no_std`. It allocates only through an allocator the consumer chose, and
allocation failure is a value, never an abort. What only nightly Rust can
express is documented as unsupported.

*Checked by:* CI on the minimum supported version, with and without `std`.

## Non-goals

- **Finding the facts.** ffibox records ownership facts in types; it does not
  infer them. Inference is the producer's: a human, an agent or an analysis.
- **Generating bindings.** ffibox is what bindings are written in, not a
  generator. Its regularity serves generators.
- **Memory the foreign side neither aliases nor lays out.** That is standard
  Rust's.
