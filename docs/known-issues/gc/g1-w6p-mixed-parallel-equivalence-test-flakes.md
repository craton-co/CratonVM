# `g1_w6p_mixed_parallel_equivalence` fails on most runs of a loaded host

- **Status (2026-09-26):** OPEN. Owner: the G1 collector round (owner of
  `gc/src/g1*.rs`). Filed 2026-09-25 by the gc-common round orchestrator (wave
  18 verification). Still seen after the filing: the w24-w32 verification
  lists it among the known G1 test flakes, next to `g1_lane_d_parallel_seed`
  (`docs/internal/gc-common-round-20260923/orchestrator-w24-w32-verification.md`).
  The test is unchanged since `1273166a2` (the function is at
  `gc/tests/g1_w6p_mixed_parallel_equivalence.rs` ~:559). The `gc/src/g1.rs`
  commits since the filing (the forwarding busy-wait bound `47a9c51ec`, and
  gc-common's identity-hash work in `6fc0e2c3f`) do not target the mixed
  driver's partition; whether the busy-wait change moves the flake rate is not
  verified. Not re-run for this status (docs-only pass).
- **Test:** `gc/tests/g1_w6p_mixed_parallel_equivalence.rs`,
  `the_two_mixed_drivers_agree_and_the_parallel_one_has_a_span_ratio`.

## Evidence

On the gc-common `fa8b0ae84` tree (wave 17) and on the wave-18 tree alike, run
alone with `cargo test -j 5 -p cratonvm-gc --test g1_w6p_mixed_parallel_equivalence`
while other sessions loaded the machine:

- `fa8b0ae84` `gc/` + `types/`: 1 pass in 4 runs;
- wave 18: 1 pass in 2 runs, and 1 pass in 3 with `types/src/mirror_pin.rs`
  reverted.

Every failure reports the same numbers:

```
rset : the two drivers disagreed about which objects to evacuate for 1784 of 6656 nodes (first few: [0, 1, 2, 3, 4, 5, 6, 7])
```

The earlier wave-17 suite run passed it. So the verdict depends on timing, not
on the tree. A stable 1784 suggests one whole region (or one remembered-set
card batch) is evacuated by one driver and not the other: a work-distribution
race in the parallel mixed driver, or a test that assumes a deterministic
partition.

## What would retire it

The owner of `gc/src/g1*.rs` either makes the parallel mixed driver's
evacuation set independent of scheduling, or makes the test compare only what
is guaranteed, and 20 consecutive runs pass on a loaded host.
