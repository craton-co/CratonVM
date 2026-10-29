# Proposal: an IR `Arrays.copyOfRange(byte[], int, int)` prefix for the callers `substring` does not cover

Status: OPEN -- IMPLEMENTED in rx27 wave 3, opt-in `CRATONVM_JIT_IR_ARRAYS_COPY_OF_RANGE_PREFIX=1`, pending its
host A/B (arms and greps: the "Round rx27 wave 3 (lane splitfast)" section of
`r23-w5-compiled-jdk-string-split-and-regex-bodies-7-13x-20261021.md`). Was: proposal. Area: JIT optimizing tier, String / array prefixes
(`jit/src/ir_lower.rs`, `jit/src/x64/concat_emit.rs`). Severity: LOW (performance only). Filed by round rx27
lane splitfast, 2026-10-28.

## Why it is not the rx27 change

The rx27 brief named an IR `copyOfRange([BII)` lowering as the likely lever for the split rows. Lane splitfast
took the larger cut instead: `CRATONVM_JIT_IR_STRING_SUBSTRING_PREFIX` /
`CRATONVM_JIT_IR_SEQ_SUBSEQUENCE_PREFIX` answer `substring` / `subSequence` whole (one bump for the `byte[]` and
the String), so `StringLatin1.newString` -> `copyOfRange` -> `new String` never runs on those sites (see the
rx27 section of `r23-w5-compiled-jdk-string-split-and-regex-bodies-7-13x-20261021.md`).

## What is left

Other callers still reach `Arrays.copyOfRange([BII)`: `StringLatin1.newString` from `String(byte[], ..)`
paths, `StringBuilder.toString` when its prefix declines, `Arrays.copyOfRange` in user code. Its optimizing body
was measured slower than its single-pass body (r23 wave 13: `CRATONVM_JIT_STRING_PIN_SKIPS_COLD_THROW_ARMS`
lost), so that pin skip stays opt-in.

## What would close it

A `JavaUtilArrays`-style prefix kind for the static `copyOfRange([BII)[B` (like `ArraysEquals`): a non-null
`byte[]`, `0 <= from <= length`, `from <= to`, a fresh `byte[to - from]` bumped inline (a bare-`byte[]` variant
of `emit_ir_to_char_array`'s bump), `min(length - from, to - from)` bytes copied and the tail left zero; the
whole-array case (`from == 0 && to == length`) is the same answer (`clone()` is a fresh equal array). Every
other input declines to the call, which throws `IllegalArgumentException` ("4 > 2") or the
`System.arraycopy` `ArrayIndexOutOfBoundsException` exactly as now. `tools/probes/jit/rx27/Rx27SplitFastFacts.java`
already prints every one of those edges (`fact copyofrange ...`) for the parity diff.
