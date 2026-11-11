# Patterns

Beyond ownership, a C API follows recurring patterns. Each is reached as the
standard Rust trait or idiom it corresponds to, from the C routines that
implement it.

| C pattern | Rust | ffibox |
|-----------|------|--------|
| a NULL-terminated array of pointers (`char **argv`, `foo **list`) | `Iterator` | `iter::CNullTerminated`, of `&CStr`s or `FooRef`s |
| an array and its count | slices | `CVec` derefs to `[T]` for plain data and owners; `as_handles` / `CSlice` for C objects |
| a `count` and a `get(i)`; a `->next` chain; a cursor advanced by `next(it)` | `Iterator` | none: `(0..n).map(get)`, `iter::successors`, `iter::from_fn` over the safe getter |
| a negative return as the error code | `Result`, with `?` | `status::nonneg` |
| zero for success, a code otherwise | `Result`, with `?` | `status::zero` |
| NULL for failure, the error kept elsewhere | `Result` | `status::non_null` |
| `-1` for failure, the error in `errno` | `io::Result` | `status::errno` (`std`) |
| an object returned through an out-parameter | a return value | `CBox::from_c_out`, `CStrBox::from_raw_out` |
| a `foreach(callback, context)` visitor | a closure | `CClosure` |
| a callback the library keeps | a boxed closure | `CPayload` |
| a callback the library hands Rust to call | a closure | `CFn`, or `CFnBox` with its context's release |
| a callback that is a Rust closure or a C function, whichever supplied it | `impl FnMut`, `Box<dyn Fn>` | `CCallback`, `CCallbackBox` |

The `status` functions return the code itself as the error, so `?` converts
it into the binding's error type through its `From` impl:

```rust,ignore
let n = status::nonneg(unsafe { sys::foo_read(f.as_mut_ptr(), buf, len) })?;
```

Not yet covered: read and write callback pairs as `Read` / `Write` / `Seek`,
option setters as builders, and structs of function pointers as traits the
Rust side implements.
