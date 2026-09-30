# G1's zeroed-header screens read `num_slots()` before the mark, and so follow a forwarded-looking mark out of the heap

*Filed 2026-09-27 by the GC defects round, wave d1, lane e (`gcd d1/e`). Found
by reading while auditing the pin9 page's item 3. Not reproduced. G1 is out of
this round's scope, so nothing was changed.*

- **Status:** OPEN.
- **Severity:** a possible SIGSEGV in G1 diagnostics and in one G1 evacuation
  proof, on bytes that are not an object whose second 4-byte word happens to
  read as a FORWARDED, not-SELF, not-BUSY mark (the ASLR-dependent shape of
  `../../internal/gc/gengc-r5w5-pin9-header-screens-follow-forwards-out-of-the-heap-FIXED-20260928.md`).
- **Backend:** G1.
- **Owner:** the G1 owners.

## What is wrong

`ObjectHeader::num_slots()` and `array_length()` resolve a FORWARDED mark
through the header's second word (`resolved_shape` -> `shape_source` ->
`forwarding_address`), trusting only `plausible_heap_pointer`. A screen that
asks "is this a zeroed header?" about bytes that may not be an object must
therefore prove the mark is not forwarded (or zero) BEFORE calling either.
Three G1 screens call `num_slots()` first:

| Site (`gc/src/g1.rs`, at `6d39e8dcc`) | Order |
|---|---|
| ~14179, the empty-header grid proof (`g1_evac_empty_header_grid_proof`) | `class_id == 0 && num_slots() == 0 && kind() == Object && ...` |
| ~16981, `is_zeroed` in a stale-reference report | `class_id == 0 && num_slots() == 0 && kind_tag(mark) == 0 && array_length() == 0` |
| ~17100, a second zeroed predicate | `class_id == 0 && num_slots() == 0 && kind_tag(mark) == 0 && array_length() == 0 && mark == 0` |

A class-id-0 word followed by a mark with state `0b11` and SELF/BUSY clear
sends `num_slots()` to whatever the next word names.

The Generational twin (`gen_heap.rs` `header_is_zero`, and its copy in
`dead_young_ref_reason_global`) had the same order and was fixed in gcd d1/e by
testing `mark_word == 0` first; see the pin9 page's STATUS.

## Proposed fix (XS)

In each predicate, test `mark_word.load(Relaxed) == 0` (or, where a non-zero
mark is legal, `!ObjectHeader::is_forwarded_mark(mark)`) before `num_slots()`.
The conjunction is unchanged, so every non-faulting answer is unchanged.

## How to verify

A G1 unit test in the shape of `gen_heap`'s
`header_is_zero_tests_the_mark_before_following_a_forward`: bytes with
`class_id = 0`, `mark = 0x7673`, second word `0x1_0000_0000` inside a live
array's data; each predicate answers "not zeroed" without faulting.
