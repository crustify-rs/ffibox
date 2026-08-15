# crustify-prim

- Skill name: crustify-prim
- Doc path: README.md
- Description: Choose and apply the right crustify smart pointer / trait when
  representing a C type's ownership and lifetime in safe Rust. Use when
  wrapping a raw C pointer or by-value C struct, or when porting a C allocator,
  constructor, or destructor — i.e. when deciding among CBox / CBoxWith / CVal /
  CVec / CVoidBox / CrustifyStr / CValGuard / CKeepalive / CTethered / CSlice /
  COut and the traits that drive them (CDropped / CCloned / CValued /
  CLenDropped / CElem / Owner, plus the fat-owner strategies CDropper /
  CCloner). Also covers the three types every wrapped C
  type gets — the layout newtype over CType, plus the NRef<'a> / NMut<'a>
  borrowed handles that carry the accessors — emitted together by
  define_ctype!, and the rule they exist for: no reference to a wrapped C
  object is ever formed. The README named below is the decision material:
  three axes — type representation, lifetime contracts, pointer representation
  — and a decision procedure that walks from a C declaration to the wrapper it
  wants. Each API then carries a worked example in its own rustdoc:
  `src/owned_refs.rs`, `src/borrowed_refs.rs`, `src/traits.rs`,
  `src/macros.rs`, `src/c_type.rs`.

`Doc path` is relative to this file, so it resolves wherever the checkout sits.
