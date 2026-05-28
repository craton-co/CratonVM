# BC EC `Mod.modOddInverse` residual failures — open investigation

## Status

**Open.** bc-math-ec runs at 6/14 + 2 fails + 6 errors with JIT enabled
(skip-list applied for the SEGV-triggering family of methods). The
proposed fix from a 2026-05-28 background agent was applied, found to
be incomplete (the same long-bit collision survives at a second loss
point downstream from the patched site), and reverted in the same
session.

## What the agent found

`CompactValue::to_value()` is lossy for longs whose bits 49..47 form
`SUB_INT` (`000`) or `SUB_UNINIT` (`100`) with payload `0`:

- A long `0xFFFC_0000_0000_0000` (which BC's safegcd accumulators
  `Mod.updateDE30` / `updateFG30` hit routinely) is NaN-tagged at the
  observable bit pattern level.
- `to_value()` reads the sub-tag, sees `SUB_INT`, and returns
  `Value::Int(0)`.
- The fast-path `lload_N` opcodes (`0x1e`..`0x21`) and the wide
  `lload`/`dload` (`0x16`/`0x18`) route the local through
  `frame.get_local_unchecked()`, which calls `to_value()` and then
  `push_unchecked()`, which calls `from_value()` and re-encodes. For
  the collision patterns above, the round-trip drops the long value.

The agent identified `Mod.updateDE30` and `updateFG30` as the BC code
that exercises this. Both methods are tight loops doing
`(long)u * di + (long)v * ei + (long)mi * md` style accumulator
updates that frequently land in the `0xFFFx_0000_0000_0000` band when
the high two bytes of a fully-signed `int * int` product are `0xFFFC`
or `0xFFFE`.

## Why the proposed fix didn't close it

The agent's proposed change: replace the lossy `get_local_unchecked` +
`push_unchecked` chain with bit-exact `get_local_compact_unchecked` +
`push_compact`. This was applied, built, and tested.

**Outcome**: bc-math-ec went from 6/14 + errors (3 m 38 s, completing)
to rc=127 SEGV at 2 m 43 s with empty stdout. Reproducible with both
JIT on and `CRATONVM_DISABLE_JIT=1`. The fix made things strictly
worse.

**Why**: `pop_long` (`vm/src/runtime/value_stack.rs:626`) reads the
slot's NaN-tag and dispatches:

```rust
match cv.tag() {
    CompactTag::Int => {
        // KC26 K1: bytecode may have left an int where a long was
        // expected; sign-extend.
        Ok(cv.as_int().unwrap_or(0) as i64)
    }
    _ => Ok(cv.as_long_unchecked()),
}
```

For a raw long with bits `0xFFFC_0000_0000_0000`, `cv.tag()` reports
`CompactTag::Int` (because the NaN-tag mask matches and the sub-tag is
`000`). The `Int → widen` branch fires, reads the payload as `0i32`,
sign-extends to `0i64`, and returns that. The long value is dropped at
this second stage exactly the way it was dropped at the first.

So the agent's fix moved the bit-loss site by one stack op but did not
eliminate it. The downstream `pop_long` is the actual load-bearing
loss; until that's also addressed, patching `lload` alone changes
nothing for the failing tests but may destabilise something else
(empirically: it did).

The destabilisation root cause wasn't pinned down before the revert —
the working hypothesis is that some non-collision long now arrives at
a consumer with different (but valid) tag bits than before, and that
consumer's tag-dispatch picks a path it didn't pick under the old
chain. Empty-stdout SEGV at 2 m 43 s without a stack trace makes
narrower diagnosis a multi-hour investigation in its own right.

## The whole loss model

There are at least three stages a `long` value transits in the
interpreter where the NaN-box collision can drop it:

1. **`lstore_N` → slot.** Already bit-exact via `pop_compact` +
   `set_local_unchecked` → `from_value(Value::Long(v))` →
   `CompactValue::long(v) = Self(v as u64)`. The bits land raw in the
   slot.

2. **slot → operand stack via `lload_N`.** Currently goes through
   `to_value()` → `from_value()`. Lossy for the collision patterns.
   This is the stage the agent patched.

3. **operand stack → consumer via `pop_long`.** Currently dispatches
   on `cv.tag()` and widens the `Int` arm. Lossy for the *same*
   collision patterns even when the previous stage was bit-exact.

A complete fix needs to either:

- (a) Make all three stages bit-exact for longs. Stage 3 needs the
  most care because the comment says int-widening is preserved for
  bytecode that the verifier accepts but that leaves an int on the
  stack where a long is expected. Distinguishing "int the verifier
  allowed to widen" from "long whose bits coincidentally tag as Int"
  requires either a parallel type tag on the stack (the way the
  parallel `stack_oop_marks` vec works for the JIT) or a re-encoding
  pass that retags collision-long bits to `SUB_LONG_LO`/
  `SUB_LONG_HI`.

- (b) Eliminate the collision space. Reserve a NaN-tag pattern as
  "explicit long" and always encode longs there. Currently
  `CompactValue::long(v) = Self(v as u64)` is bit-verbatim; a
  collision-free encoding would XOR or shift the bits into a region
  that doesn't overlap `SUB_INT`/`SUB_UNINIT`/`SUB_OBJECT`/etc. That
  costs one ALU op per long load/store but closes the bug class.

Neither is a one-line change. Both touch hot paths that need careful
profiling.

## Out of scope until prerequisites land

- bc-math-ec residual 6 errors + 2 failures.
- `--nojit` 7/14 baseline (same root cause).
- Any other place where `(long)a * b` accumulators hit the
  `0xFFFx_0000_0000_0000` band — likely some Lucene / Tika / Jackson
  numeric paths too, though we haven't measured them.

## What to do when revisiting

1. Audit every `pop_long` / `pop_double` / `peek_*` call site in
   `value_stack.rs` for the same shape. The agent only flagged
   `pop_long`; `pop_double` has a sibling switch on `cv.tag()` at
   line 679 that may have the same hazard.

2. Decide between (a) parallel type tags vs (b) collision-free
   encoding. Sketch the perf impact of each via a microbenchmark
   (a tight `lload_0; lstore_0` loop is enough to see the per-op
   delta).

3. Land the chosen fix end-to-end (slot → stack → consumer) in one
   commit so the lifecycle stays consistent. Don't try the agent's
   "fix one stage at a time" approach — it leaves the other stages
   inconsistent and can SEGV (as demonstrated).

4. Re-run bc-math-ec with JIT on; expect 14/14 if the diagnosis is
   right. Re-run with `CRATONVM_DISABLE_JIT=1` too — the bug is
   purely interpreter-side, so both modes should pass.

5. Update `test-infra/suite-results/history.tsv` with the new
   numbers.

## Related fixes in this area

- `types/src/compact_value.rs` `to_value()` — already has rescue
  heuristics for `SUB_OBJECT` (unaligned ptr = long), `SUB_NULL`
  (non-zero payload = long), `SUB_RETADDR` (high bits set = long).
  `SUB_INT` and `SUB_UNINIT` with payload `0` are the remaining
  unsalvageable cases because the payload `0` is ambiguous between
  "real Int(0)" / "real Uninit" and "long with payload-region all
  zero." A type tag is the only way to disambiguate, hence (a) above.
