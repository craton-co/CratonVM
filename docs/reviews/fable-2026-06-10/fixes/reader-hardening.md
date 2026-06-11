# Fix-note: reader-hardening

**ID:** reader-hardening
**Report:** docs/reviews/fable-2026-06-10/reader.md
**Owned files:** reader/src/buffer.rs, reader/src/stack_map.rs, reader/src/class_reader.rs

## Finding

The reader parses untrusted classfiles. Most length/offset arithmetic was migrated
to `checked_add` in prior audit rounds, but the report (reader.md) flagged one
remaining hand-rolled `pos + N` bounds check that lacked the crate-standard
overflow hardening:

- **B2 (low)** — `reader/src/stack_map.rs:224`: `read_u16` used the bare compare
  `if *pos + 2 > data.len()`. If `*pos` is ever near `usize::MAX`, `*pos + 2`
  wraps to a small value that passes the bounds check, then `data[*pos]` /
  `data[*pos + 1]` index out of bounds (panic). Not reachable today (`pos` starts
  at 0 and only advances by small bounded reads), but it is the exact anti-pattern
  every other reader (`buffer.rs::read_bytes`, `instruction.rs`) was standardised
  away from, and a future caller seeding `pos` from an external offset would
  reintroduce the wrap.

## Root cause

`read_u16` predates the round-7 `checked_add` migration and was never updated; its
sibling `read_u8` (line 213) already uses the safe `*pos >= data.len()` form.

## Exact change

`reader/src/stack_map.rs` — `read_u16` now computes the end offset with
`pos.checked_add(2)` and only proceeds when `Some(end)` with `end <= data.len()`;
overflow or out-of-bounds both return the same `InvalidClassData` "unexpected end
of data" error as before. The successful `*pos = end` write reuses the checked
value. Behavior for all valid input is byte-identical (same error type, same
advance); only the malformed/near-`usize::MAX` edge changed from potential
wrap+panic to a clean parse error.

Added two `#[cfg(test)]` regression tests in the existing `stack_map.rs` test
module:
- `read_u16_overflow_returns_error` — drives `pos = usize::MAX - 1`, asserts
  `InvalidClassData` and that `pos` is left untouched (so a retry re-observes the
  error).
- `read_u16_boundary` — confirms the exact-boundary read (`*pos + 2 == len`)
  still succeeds and one-byte-short is rejected, pinning the off-by-one.

## Sites reviewed and deliberately left unchanged

- **`buffer.rs` fixed-width reads** (`read_u8/u16/u32/i64`, `pos..pos + N`): `N`
  is a compile-time constant (1/2/4/8), not an untrusted length, and `position`
  is structurally always `<= data.len()` (every write is a `checked_add`-validated
  `end`). The report explicitly classes `buffer.rs` as "well hardened" and did not
  flag these. No change (no behavior delta, not the flagged anti-pattern).
- **`class_reader.rs:509` `start + length`**: guarded — line 486 already rejects
  `length > buf.remaining()`, and the preceding `buf.read_bytes(length)?` (which
  uses `checked_add` internally) makes `start + length == buf.position() <=
  source.len()`. Cannot overflow; not flagged. Left unchanged.
- **`class_reader.rs:229/244` `i + 1 >= count`**: `i` and `count` are `u16` with
  the loop invariant `i < count <= 65535`, so `i + 1 <= 65535` never overflows
  `u16`. Not flagged. Left unchanged.

## Files touched

- `reader/src/stack_map.rs` — `read_u16` hardened to `checked_add` + 2 regression tests.

## Tests added

Yes — `read_u16_overflow_returns_error`, `read_u16_boundary` (both in
`reader/src/stack_map.rs`'s `#[cfg(test)] mod tests`).

## Follow-up & risk

- **B1 (medium, NOT in my owned files)** — the cached signature-parse depth-guard
  bypass lives in `reader/src/signature.rs:530/553/576` (the three
  `parse_*_signature_cached` fns discard the `SigParser` and never read
  `depth_exceeded`, so a depth-bombed signature returns/caches a partial AST while
  the uncached API returns `None`). I do not own `signature.rs`, so I could not fix
  it. Recommended fix per the report: bind `let mut p = SigParser::new(sig);`,
  parse, then treat `p.depth_exceeded` as a parse failure before inserting
  `Class/Method/Field` vs `Invalid` into the cache. **Needs a separate owner of
  `signature.rs`.**
- **i32 switch overflow** — the report notes the crafted `high=i32::MAX,
  low=i32::MIN+1` tableswitch is *already* rejected before allocation at
  `instruction.rs:519-528` with an existing regression test; `instruction.rs` is
  not in my owned set and needs no change.

**Risk:** minimal. The change is a strict tightening of a malformed-input edge;
valid input produces the identical `Result` (same Ok value / same error variant).
No public API change. Confident it compiles (mirrors the existing `read_u8` guard
style and `buffer.rs` `checked_add` pattern; `ClassReaderError::InvalidClassData`
and the test imports are already in scope).
