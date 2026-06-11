# CratonVM `reader` crate — code & test review (Fable, 2026-06-10)

Scope: `reader/src` (18 files, ~10.3k LOC) + `reader/tests` (5 files). This crate
parses Java `.class` files, the bytecode instruction stream, the generic-signature
grammar, type descriptors, and the jimage (`$JAVA_HOME/lib/modules`) container. **All
input is untrusted** (classfiles, jars, network-loaded bytes, the on-disk runtime image),
so the bar is: no panic, no UB, no OOM, no unbounded recursion on any byte sequence.

## Summary

Overall the crate is in **good shape** and shows the scars of multiple prior security
audit rounds (round-4 through round-11 + "Round 7/8/9 audit fix" annotations). The hot
read primitives (`buffer.rs`, `byte_view.rs`) are well hardened: `checked_add` everywhere
a length is derived from untrusted bytes, a panicking `ByteView::new` deprecated and
narrowed to `pub(crate)` with a checked `try_new` on every runtime-offset call site,
recursion depth caps on the three recursive grammars (annotations 256, signatures 256,
array dims 255), and explicit pre-allocation caps + per-section count validation against
the u16 spec maxima. There are no `todo!`/`unimplemented!`/`NotImplemented` in production
code and no synthetic/fake-value natives (this crate is a pure parser — the "synthetic
stub" policy is not really applicable here).

The findings below are mostly **low/medium**: a genuine but hard-to-reach
cached-vs-uncached signature-parse inconsistency (depth-guard bypass), a couple of
hand-rolled `pos + N` bounds checks that lack the `checked_add` hardening the rest of the
crate standardised on, and one documented unsupported feature (compressed jimage
resources) that is a real functional gap for some production JDKs. No memory-safety or
unsoundness defects were found; there are no `unsafe` blocks in the crate.

## Bugs

### B1 (medium) — cached signature parse bypasses the depth-exceeded guard
`reader/src/signature.rs:530, 553, 576`

The non-cached entry points latch and honor the sticky `depth_exceeded` flag:
```rust
pub fn parse_class_signature(sig: &str) -> Option<ClassSig> {
    let mut p = SigParser::new(sig);
    let r = p.parse_class_sig();
    if p.depth_exceeded { None } else { r }   // line 413
}
```
The cached variants do **not**:
```rust
let parsed = SigParser::new(sig).parse_class_sig();   // line 530 — discards the parser, never reads depth_exceeded
```
For a hostile signature that nests past `MAX_SIG_DEPTH` (e.g. `Lp<Lp<...>;>;`), the
uncached API returns `None`, but the cached API returns `Some(partial_ast)` — and then
**caches that partial parse** keyed on the signature string, so every subsequent lookup
(including via the uncached path if it ever consults the cache) gets the wrong verdict.
This is an observable correctness divergence between two APIs that are documented as
equivalent, and it defeats the very DoS guard the sticky flag was added for. Same defect
in all three cached functions (`parse_class_signature_cached`,
`parse_method_signature_cached`, `parse_field_signature_cached`). Fix: capture the parser
in a `let mut p = SigParser::new(sig);`, parse, then treat `p.depth_exceeded` as a parse
failure before inserting `Class/Method/Field` vs `Invalid`.

### B2 (low) — hand-rolled `*pos + 2` bounds check in stack_map.rs lacks overflow hardening
`reader/src/stack_map.rs:224`

```rust
fn read_u16(data: &[u8], pos: &mut usize) -> Result<u16, ClassReaderError> {
    if *pos + 2 > data.len() {   // can wrap if *pos is near usize::MAX
```
Every other reader (`buffer.rs`, `instruction.rs`) was migrated to `checked_add` precisely
to avoid this pattern (see the round-7 comments). Here `pos` starts at 0 and only advances
by small reads bounded by `data.len()`, so it cannot actually reach near `usize::MAX` —
not currently exploitable — but it is the exact anti-pattern the rest of the crate
standardised away from, and a future caller that seeds `pos` from an external offset would
reintroduce a wrap. Low severity; flagged for consistency/defense-in-depth. (The sibling
`read_u8` at line 213 uses `>=` and is fine.)

### B3 (low) — `decode_location` value accumulator silently truncates >8-byte values' semantics
`reader/src/jimage.rs:393, 405-408`

`length = ((header_byte & 0x07) as usize) + 1` is in `1..=8`, and the value is accumulated
into a `u64` via `(value << 8) | byte`. That is correct for `length <= 8`. The encoding can
never express >8 here (3 low bits cap it at 8), so this is safe — but note the
`test_builder::append_attr` asserts `length <= 8` (jimage.rs:1066) while the *reader* simply
trusts the 3-bit field; a real jlink that ever emitted a wider value would be silently
mis-decoded rather than rejected. Not reachable from the current format; documented here so
it is not mistaken for a checked path.

### B4 (low, informational) — `ByteView::remaining`/`len` underflow is structurally prevented, not asserted
`reader/src/buffer.rs:24-26`, `reader/src/byte_view.rs:165-167`

`ClassFileBuffer::remaining()` computes `self.data.len() - self.position` and
`ByteView::len()` computes `self.end - self.start` without a guard. Both are safe **given
the invariants the constructors enforce** (position never advances past `data.len()`;
`try_new`/`new` reject `start > end` and `end > len`). No bug today, but these are the kind
of subtraction that becomes a panic if an invariant is ever weakened; a `debug_assert` would
make the invariant load-bearing in tests.

## Vulnerabilities

No memory-safety or unsoundness vulnerabilities were found. The crate contains **zero
`unsafe` blocks**. The historically dangerous surfaces are all covered:

- **Pre-allocation DoS** — every `Vec::with_capacity(n)` clamps to `PREALLOC_CAP` (1024) or
  is bounded by a u16 count; switch tables clamp to `MAX_SWITCH_ENTRIES` (16384);
  StackMapTable clamps to 65535. A crafted `high=i32::MAX, low=i32::MIN+1` tableswitch is
  rejected before allocation (`instruction.rs:519-528`), with a regression test.
- **Length-times-entry-size overflow** — bulk parsers compute `count * ENTRY_SIZE` where
  `count <= 65535` and `ENTRY_SIZE <= 10`, so the product never exceeds ~655k; `read_bytes`
  uses `checked_add` and bounds-checks the slice (`buffer.rs:102-112`).
- **Stack-overflow via recursion** — annotation/element-value (256), signature (256), array
  dimensions (255) are all depth-capped with dedicated regression tests
  (`deeply_nested_*`).
- **Attribute-length / offset OOB** — `read_attributes` rejects `length > remaining` before
  slicing (`class_reader.rs:486`); all runtime-derived `ByteView` ranges go through
  `try_new` (StackMapTable, Code, Unknown).
- **jimage** — header, every section offset, every table index, string offsets, and
  resource ranges are `checked_*` / `.get(..)` guarded and return typed errors
  (`jimage.rs:507-542, 608-723`).
- **CESU-8 decode** — routed through the `cesu8` crate with a typed error on failure
  (`class_reader.rs:209`), not raw `from_utf8_unchecked`.

The only residual item worth a security eye is **B1** (a DoS guard that is bypassed on the
cached path), classified as a bug above rather than a standalone vuln because it requires
the caller to use the cached API.

## Stubs and Unimplemented

No `todo!`/`unimplemented!`/`NotImplemented`/no-op fakes in production code. One genuine,
**documented** functional gap:

- **Compressed jimage resources are unsupported** — `reader/src/jimage.rs:704-706` returns
  `JImageError::Compressed(path)` for any entry with `compressed_size > 0`. Production JDKs
  default to `--compress=0` (uncompressed), so this is usually fine, but a JDK built/relinked
  with jlink compression (Zstd) would be unreadable. This is the closest thing to a "stub":
  the reader correctly *errors* rather than faking data, which is policy-compliant, but it is
  a real capability gap (see Feature F1).

(All `unreachable!`/`unwrap`/`expect` occurrences in `src/` are either in `#[cfg(test)]`
modules or provably-safe: the `try_into().unwrap()` in `buffer.rs` follows an explicit
length check, and the `unreachable!("decoded above")` in `LazyAttribute::decode` follows an
unconditional assignment to the `Decoded` variant.)

## Performance

The crate has clearly been through allocation tuning (zero-copy `ByteView`, interned
`Arc<str>` constant-pool names, bulk-slice attribute parsers, FIFO signature cache). Items
below are minor.

### P1 — `decode_attribute` / `decode_attribute_with_source` re-intern the name on every call
`reader/src/attribute.rs:835, 861`

Both eager entry points call `cratonvm_types::intern_arc(name)` (a global
`Mutex<HashMap>` lookup) per attribute to obtain the canonical `Arc<str>` for the
`Arc::ptr_eq` dispatch fast path. The lazy hot path already avoids this by using
`decode_attribute_with_source_arc` directly, so impact is limited to callers of the `&str`
API, but for any consumer that decodes attributes in bulk via the `&str` form this is a
lock acquisition per attribute. Consider exposing the dispatch by a small `match name`
without forcing an intern, or documenting that bulk callers should pre-intern once.

### P2 — `ConstantPool::validate()` allocates a `String` per error and is O(n) with per-entry formatting
`reader/src/constant_pool.rs:187-280`

`validate()` builds a `Vec<String>` with `format!` for each violation. It is not on the
parse hot path (it's an opt-in cross-reference checker), but if a caller runs it on every
loaded class it does a fair amount of formatting. Returning structured error enums (or a
count + first-error) would avoid the per-error `String` alloc. Low priority.

### P3 — jimage `iter_entries` builds a `HashSet<usize>` and `Vec<(String,..)>` eagerly
`reader/src/jimage.rs:754-779`

`iter_entries` allocates a `String` path for every resource (tens of thousands for a real
`lib/modules`) plus a dedup `HashSet`. It's described as test/bootstrap-map support, so the
cost is paid once, but a streaming/callback variant (or returning interned `Arc<str>`)
would cut the bootstrap allocation churn if it is called during VM start.

### P4 — `test_builder::find_group_seed` is O(seed × members) brute force (test-only)
`reader/src/jimage.rs:1025-1054`

Iterates seeds `1..=100_000` rebuilding a `HashSet` per attempt. Test-support only; noted
for completeness, not a runtime concern.

## Tests

**Estimated coverage: ~78%.** Does **not** plausibly reach 85% on its own, primarily
because the bytecode instruction decoder and the jimage error paths are under-covered
relative to their risk.

Basis (what has tests vs none):
- **Well covered**: `buffer.rs` (incl. the `usize::MAX` overflow regression),
  `byte_view.rs` (OOB/inverted/`try_new` paths), `constant_pool.rs` (validate matrix),
  `field_type.rs` / `method_descriptor.rs` (incl. 255-dim cap + 100k-`[` bomb),
  `signature.rs` (depth-bomb + cache round-trip), `stack_map.rs` (every frame variant +
  u16::MAX absolute-offset overflow), `class_file_version.rs`, the access-flag/field/method
  helper structs, and `jimage.rs` round-trip + the four section-truncation/bad-magic cases.
  The integration tests (`vulnerability_fixes.rs`, `wp_large_file.rs`, `wp_switch_padding.rs`)
  are excellent: real class-file synthesis, switch-OOM, fixed-size-attribute length rejection,
  near-u16::MAX constant pool, code_length == 65535 cap.
- **Thin or missing**:
  1. **`instruction.rs` opcode coverage** — tests hit maybe ~25 of 200+ opcodes. Whole
     families (all conversions, all comparisons, all arithmetic, monitorenter/exit,
     athrow, anewarray/multianewarray dimensions, every `wide` form except iload/iinc,
     getfield/putfield/getstatic/putstatic) are untested. No test for a truncated
     tableswitch/lookupswitch *body* (count valid but bytes run out), nor for the
     `invokeinterface`/`invokedynamic` reserved-byte rejection, nor the `wide` invalid-opcode
     error arm.
  2. **`signature.rs` cache depth-guard (B1)** — there is no test asserting the cached and
     uncached APIs agree on a depth-bombed signature; such a test would have caught B1.
  3. **`jimage.rs` error paths** — `JImageError::Compressed`, `BadResourceRange`,
     `BadStringOffset` from a missing NUL terminator, and a location offset past the
     locations buffer (`find_location`'s hard-error branch) are not exercised by a crafted
     image.
  4. **`class_reader.rs` malformed constant-pool cross-refs** — e.g. `this_class` pointing at
     a non-Class entry, a Long/Double in the last CP slot (the rejection at line 229/244 is
     untested), CESU-8 decode failure, unsupported-version rejection at the `read_class`
     level.
  5. **Annotation/type-annotation decoders** — the in-crate tests cover the common shapes but
     not the depth-256 rejection through a *real* `RuntimeVisibleAnnotations` attribute, nor
     the `decode_target_info` localvar (`0x40/0x41`) table-length path.

Most important missing tests (to push toward 85%): (a) an exhaustive opcode-decode table for
`instruction.rs` including all `wide` forms and switch-body-truncation; (b) a B1 regression
asserting cached==uncached on a depth-bomb signature; (c) crafted-jimage tests for each
`JImageError` variant; (d) `read_class` constant-pool cross-reference rejection cases
(this_class→non-Class, Long-in-last-slot, bad CESU-8, unsupported version).

## Feature Suggestions

- **F1** — Support compressed jimage resources (Zstd) so a jlink-compressed `lib/modules`
  is readable instead of erroring (`jimage.rs:704`). Gate behind the existing `JImageError`
  surface; many enterprise JDK images ship compressed.
- **F2** — Add an opt-in **strict structural verifier** pass that, given a parsed
  `ClassFile`, validates bytecode branch targets land on instruction boundaries and switch
  offsets stay within the method (the `_base_pc` retained at `instruction.rs:534` hints this
  was planned). Reusable across the JIT and interpreter.
- **F3** — Make the signature cache pluggable/bounded-configurable and add a metrics hook
  (hit/miss/evict counts); the FIFO+8192 cap is currently a hard constant
  (`signature.rs:458`).
- **F4** — Provide a streaming `iter_entries` callback API for jimage to avoid materialising
  a `Vec<(String, u64, u64)>` of every resource at boot (P3).
- **F5** — Expose `ConstantPool::validate` results as structured enums rather than
  `Vec<String>` so callers can act on them programmatically and avoid per-error formatting
  (P2).
- **F6** — Fuzz harness (cargo-fuzz / libFuzzer) targeting `read_class`, `Instruction::decode`,
  `StackMapTable::parse`, the signature grammar, and `JImageReader::from_bytes`. The crate's
  whole threat model is "arbitrary bytes in, no panic/OOM"; continuous fuzzing is the natural
  fit and would harden the under-tested instruction/jimage paths.

## Files sampled vs fully read

**Fully read (every line):** `buffer.rs`, `byte_view.rs`, `class_reader.rs`,
`constant_pool.rs`, `instruction.rs`, `stack_map.rs`, `jimage.rs`, `signature.rs`,
`field_type.rs`, `method_descriptor.rs`, `class_file.rs`, `class_file_version.rs`,
`class_access_flags.rs`, `class_reader_error.rs`, `lib.rs`, `field.rs`, `method.rs`;
tests `vulnerability_fixes.rs`, `wp_large_file.rs`, and the header of `wp_switch_padding.rs`.

**Sampled (structure grep + deep-read of the risk regions):** `attribute.rs` (3288 LOC) —
read the structure via grep, then fully read the high-risk regions:
`validate_attribute_shape` + Code shape walk (696-800), the decode dispatch and offset
invariant (821-1000), the bulk attribute-body decoders (1040-1430), `decode_attributes_vec`
+ `decode_code_body` (1445-1597), the annotation/element-value/type-annotation recursive
decoders + depth caps (1599-1805), and the `LazyAttribute` machinery (470-695). The
remaining ~1400 lines of `attribute.rs` are `#[cfg(test)]` unit tests, which were skimmed
for the coverage assessment but not line-audited.

**Not deep-read:** the bulk of the test modules inside `attribute.rs` (lines ~1850-3288) and
`tests/wp1_7_attrs.rs` / `tests/wp_validate_wire_up.rs` were inventoried for coverage but not
line-by-line audited.
