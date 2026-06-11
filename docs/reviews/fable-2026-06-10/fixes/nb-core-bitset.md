# Fix: nb-core-bitset — BitSet mutators: unchecked negative/huge index (B1 / V1)

## Finding

**B1 (high) / V1 (medium DoS)** — `java.util.BitSet` mutator/accessor natives
(`set(I)`, `set(IZ)`, `set(II)`, `clear(I)`, `clear(II)`, `flip(I)`, `flip(II)`,
`get(I)`) read the Java `int` bit index and converted it directly with
`*n as usize`. A **negative** index (e.g. `-1`, `Integer.MIN_VALUE`) wraps to a
near-`usize::MAX` value, which then flows into `bs_ensure_capacity`:

```rust
let needed = ((bit_index + 64) / 64) * 64;   // bit_index ≈ usize::MAX
```

`bit_index + 64` overflows (panic in debug, wrap in release), and the derived
word count drives `ctx.new_array(Long, new_len)` toward a multi-gigabyte
allocation. Real JDK `BitSet` throws `IndexOutOfBoundsException` for a negative
index. Reachable from any app calling `bitset.set(n)` with an attacker-controlled
`int` (parsed network / classfile data) — the most concrete DoS surface in scope.

The read-side scans (`nextSetBit`/`nextClearBit`/`previousSetBit`) already clamp
with `.max(0)` / handle `from < 0`, so they have no allocation/overflow hazard;
the asymmetry was specifically in the mutators and `get`.

## Root cause

Unchecked `int → usize` sign conversion in the index-taking BitSet natives, plus
an unbounded `bit_index + 64` capacity computation that trusts the (now possibly
wrapped) index.

## Exact change

File: `native-builtins/src/phases_early.rs` (BitSet region, ~2800–3120).

1. **New validation helpers** (placed after `bs_word_count`):
   - `bs_throw_index_oob(ctx, msg) -> MethodCallFailed` — builds a *catchable*
     `java/lang/IndexOutOfBoundsException` via `new_object_initialized` with the
     `(Ljava/lang/String;)V` ctor (so `detailMessage` is set correctly regardless
     of the synthetic-vs-real Throwable field layout, and the object is GC-pinned
     across `<init>`), returning `MethodCallFailed::ExceptionThrown`. Falls back to
     a catchable `IllegalArgumentException` only if the exception object cannot be
     materialised (early boot) — never panics, never silently succeeds.
   - `bs_checked_bit_index(ctx, args, idx) -> Result<usize, MethodCallFailed>` —
     reads a single `int` index; `n < 0` → throw; otherwise `n as usize`.
   - `bs_checked_range(ctx, args, from_idx, to_idx)` — validates a `(from, to)`
     pair: `from < 0`, `to < 0`, or `from > to` each throw `IndexOutOfBoundsException`
     (matches JDK `BitSet` range semantics).
   - `const BS_MAX_WORDS` = word count for `Integer.MAX_VALUE` (the largest legal
     bit index), used to bound the backing allocation.

2. **Rewired every index-taking mutator/accessor** to use the helpers:
   `native_bs_set`, `native_bs_set_val`, `native_bs_clear`, `native_bs_flip`,
   `native_bs_get` → `bs_checked_bit_index`; `native_bs_set_range`,
   `native_bs_clear_range`, `native_bs_flip_range` → `bs_checked_range`.

3. **Hardened `bs_ensure_capacity`** as a defence-in-depth backstop: compute the
   requested word count via `bs_word_count(bit_index.saturating_add(1))`, clamp it
   with `.min(BS_MAX_WORDS)`, and derive `needed` with `saturating_mul(64)` — so
   even a stray oversized index can never overflow `bit_index + 64` nor request an
   unbounded `new_array`. The rounding semantics are identical to the old
   `((bit_index + 64) / 64) * 64` for all in-range indices (verified for boundary
   cases 0, 63, 64, 127).

Valid (non-negative) indices behave exactly as before.

## Files touched

- `native-builtins/src/phases_early.rs` — helpers + 8 native rewires +
  `bs_ensure_capacity` hardening + 10 `#[cfg(test)]` tests.

## Tests added

In the existing `t2_tests` module (driven through `mock_ctx`):
- `bs_set_negative_index_throws`, `bs_clear_negative_index_throws`,
  `bs_flip_negative_index_throws`, `bs_get_negative_index_throws` — each asserts
  `Err(ExceptionThrown)`.
- `bs_set_min_int_index_does_not_panic_and_throws` — the `i32::MIN` wrap case that
  previously drove the overflow/giant-alloc; now a clean throw.
- `bs_set_range_negative_from_throws`, `bs_set_range_from_greater_than_to_throws`.
- `bs_set_then_get_valid_index_roundtrips` — non-negative path (incl. word-array
  growth to bit 130) still works.
- `bs_max_words_bounds_largest_int_index` — `BS_MAX_WORDS` exactly covers
  `Integer.MAX_VALUE`.

The mock's default `new_object_initialized` (new_object + invoke) returns a real
object, so `bs_throw_index_oob` yields `ExceptionThrown` under test.

## Follow-up & risk

- Risk: **low**. Pure validation added on the error path; the happy path is
  unchanged. The thrown exception is a genuine catchable Java `Throwable`, so apps
  that already `catch (IndexOutOfBoundsException)` around BitSet (matching the JDK)
  now behave correctly instead of crashing/hanging.
- At the absolute boundary (`set(Integer.MAX_VALUE)`), the stored `nbits`/`needed`
  still round-trips through an `i32` cast (pre-existing behavior, untouched) and
  would attempt a ~256 MiB `long[]` — the JDK's own reachable limit. Not the DoS
  vector fixed here (the negative→usize::MAX wrap is fully blocked).
- `nextSetBit`/`nextClearBit`/`previousSetBit` keep their existing lenient
  `.max(0)` clamp (no allocation hazard); the JDK throws IOOBE there too, but that
  is out of scope for this change and carries no crash/DoS risk.
- Suggested broader follow-up (from the review): a shared `bit_index_arg` helper
  and an audit of other index-taking natives for the same `int → usize` pattern.
