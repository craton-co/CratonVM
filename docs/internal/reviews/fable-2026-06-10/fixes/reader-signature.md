# Fix note: reader-signature (B1) — cached signature parse bypassed the depth-exceeded guard

- **ID:** reader-signature
- **Severity:** medium (DoS-guard bypass / cached-vs-uncached correctness divergence)
- **File changed:** `reader/src/signature.rs` (owned)
- **Source finding:** `docs/reviews/fable-2026-06-10/reader.md` § B1

## The bug

All signature input is untrusted (classfiles, jars, network bytes). The recursive
generic-signature grammar is depth-capped at `MAX_SIG_DEPTH` to prevent stack-overflow /
recursion DoS. The cap works via a *sticky* `SigParser::depth_exceeded` flag: when nesting
trips the cap, `parse_type_sig` (signature.rs:247-250) latches the flag and returns `None`,
but because `parse_type_args` intentionally `break`s on an inner `None`, the *outermost*
`parse_class_type_sig` can still return `Some(partial_ast)`. The uncached entry points
therefore explicitly consult the flag:

```rust
pub fn parse_field_signature(sig: &str) -> Option<TypeSig> {
    let mut p = SigParser::new(sig);
    let r = p.parse_type_sig();
    if p.depth_exceeded { None } else { r }   // honors the guard
}
```

The three **cached** entry points did not. They constructed the parser as a temporary,
parsed, and **discarded the parser** without ever reading `depth_exceeded`:

```rust
let parsed = SigParser::new(sig).parse_class_sig();   // parser dropped — depth_exceeded lost
```

Consequence: a hostile signature that nests past `MAX_SIG_DEPTH`
(e.g. `Lp<Lp<...>;>;`) was **rejected** by `parse_field_signature` (`None`) but
**accepted** by `parse_field_signature_cached` (`Some(partial_ast)`) — and then the partial
parse was *memoized in the global cache* keyed on the signature string, so every subsequent
probe (including reflective generics consumers that share the cache) got the wrong verdict.
Two documented-equivalent APIs diverged, and the very DoS guard the sticky flag exists for
was bypassed on the cached path.

Affected functions: `parse_class_signature_cached` (~line 530),
`parse_method_signature_cached` (~line 553), `parse_field_signature_cached` (~line 576).

## The fix

In each of the three cached functions, capture the parser, parse, then fold
`depth_exceeded` into the result exactly as the uncached path does, *before* the
cache-insert match. A depth-exceeded parse is now treated as a parse failure and cached as
`ParsedSignature::Invalid`, so cached and uncached paths agree and the memoized verdict is
correct:

```rust
let mut p = SigParser::new(sig);
let parsed = p.parse_class_sig();
let parsed = if p.depth_exceeded { None } else { parsed };
// ... existing match on `parsed` (Some -> cache the AST, None -> cache Invalid)
```

This is a behavior-preserving change for all well-formed (non-depth-bombed) signatures:
for those, `depth_exceeded` is `false`, so `parsed` is unchanged and the existing
Some/None caching paths run exactly as before. The only behavioral change is that a
depth-bombed signature now returns `None` (and caches `Invalid`) on the cached path — which
is the documented, correct behavior already produced by the uncached path.

The `Some(_)` "shape mismatch" fall-through arms were already safe: they delegate to the
uncached `parse_*_signature` functions, which honor `depth_exceeded`.

## Test added

`cached_and_uncached_agree_on_depth_bomb` (in the existing `#[cfg(test)] mod tests`):
builds a `Lp<Lp<...>;>;` signature nested to 100,000 levels (a unique trailing-comment
marker avoids racing on the shared global cache), asserts the **uncached** path rejects it
(ground truth), then asserts the **cached** path rejects it on both the first (parsing)
call and the second (memoized) call. This directly reproduces the B1 divergence and would
fail against the pre-fix code (which returned `Some` on the cached path). Mirrors the
construction used by the existing `deeply_nested_signature_is_rejected_not_overflow` test.

## Scope / constraints honored

- Edited only the owned file `reader/src/signature.rs` plus this fix-note.
- No new types/APIs; reused the existing `SigParser`, `depth_exceeded` field (same module,
  in-scope), and `ParsedSignature::Invalid` cache variant. No `unsafe`, no new deps.
- Did not run cargo/git. Change is purely local to three functions + one test; mirrors the
  exact pattern of the three uncached entry points, so it compiles against the existing
  types.
