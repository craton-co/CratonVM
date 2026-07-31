# Committed heap exceeded `-Xmx`: the young semi-space expanded 4x with no total-heap budget

## Status
**FIXED** (2026-07-31, branch `fix/xmx-heap-budget-20260731`). `-Xmx` now bounds
the whole committed Java heap: `young_from + young_to + old_gen <= Xmx`, at the
growth ceiling and not merely at construction.

## Symptom

`-Xmx N` did not bound the process. Measured at `--Xmx 1g`:

* `young_from` 512 MiB + `young_to` **1024 MiB** + old 512 MiB = 2 GiB of Java
  heap for a 1 GiB `-Xmx`,
* **4.0 GiB RSS / 7.5 GiB virtual**, killed by the Linux OOM killer
  (`dmesg`: `oom-kill: ... task=cratonvm-oom-fi, anon-rss:4054336kB`) — no Java
  `OutOfMemoryError`, no crash report, nothing catchable.

With the 1/4-of-RAM ergonomic default (`ergonomic_default_max_heap`, no explicit
`-Xmx`) the same factor scaled with the host: on a 31 GiB box the ~7.75 GiB
default admitted a ~19 GiB committed ceiling.

Found while root-causing
`docs/internal/fixed-suite-bugs/h2-suite-bugs/bug-h2-testoutofmemory-sigabrt-young-old-gen-both-exhausted-FIXED.md`,
whose `--Xmx 1g` repro was the process the OOM killer took.

## Root cause

`GenerationalHeap::with_capacity(total)` splits the budget correctly — young
semi `total/4` each, old gen `total/2` — but `with_sizes` then derived the young
*growth* ceiling from the young semi alone:

```rust
let max_young = young_semi_size.max(1024).saturating_mul(MAX_HEAP_EXPANSION_FACTOR);
```

`MAX_HEAP_EXPANSION_FACTOR` is 4, with no reference to the total or to the old
generation, so each semi could grow to `total` and the committed heap to
`2.5 * Xmx`. The adaptive-expansion path (`collect_garbage_inner` phase 6)
doubles `young_to` on a low-reclamation cycle and clamped only against that
per-semi ceiling.

## Fix (`gc/src/gen_heap.rs`)

New `max_heap_bytes` field records the `-Xmx` budget. `with_capacity` — the sole
production path, fed from `-Xmx` through `VmHeap::new_with_overrides` — derives
the growth ceiling from what is left of the budget after the (fixed,
non-expandable) old generation, split across both semi-spaces:

```rust
let max_young_semi = total.saturating_sub(old_size) / 2;
```

Both semis are counted because they swap roles every cycle and a copying
collection needs a to-space as large as the from-space it evacuates. The
expansion site re-derives and applies the same ceiling, so the invariant holds
at the point of growth and not only in constructor arithmetic.

**Nothing is given up.** Under `with_capacity`'s 50/50 split that ceiling lands
exactly on the *initial* semi — young already starts at the largest size the
budget permits, so a heap that would previously have grown into it is strictly
better off starting there. What the clamp removes is only the ability to exceed
`-Xmx`. A workload that genuinely needs more room now surfaces a catchable
`OutOfMemoryError` (what HotSpot does, and now reachable everywhere thanks to
the native-allocation unwind channel) instead of silently committing 2.5x its
stated maximum and risking an unrecoverable kernel OOM-kill.

`GenerationalHeap::new` / `with_sizes` are unchanged (`max_heap_bytes =
usize::MAX`): they take absolute arena sizes rather than a budget to divide, and
the GC's own tests drive them with deliberately tiny arenas that rely on
expansion.

## Measured

`grows` = count of `[youngstate] arena-grow` lines (`CRATONVM_DBG_YOUNGSTATE=1`);
peak RSS from `/usr/bin/time -v`. Same host, same session, interleaved arms.

| workload (`--Xmx 1g`) | grows base -> fix | peak RSS base -> fix | checksum |
|---|---|---|---|
| `HashMapOnly 5000000` | 1 -> **0** | 2,010,304 kB -> **1,744,796 kB** (-13.2%) | 387499957500000, unchanged |
| `HashMapOnly 5000000` (rep 2) | 1 -> **0** | 2,008,684 kB -> **1,725,940 kB** (-14.1%) | unchanged |
| `BinTreesClassic 18` | 0 -> 0 (expansion never fires) | 865,664 kB -> 863,480 kB | 68332206, unchanged |
| `TestOutOfMemory` (nojit) rep 2 | 3 -> **0** | 3,627,180 kB -> **2,094,208 kB** (-42%) | — |
| `TestOutOfMemory` (nojit) rep 3 | 2 -> **0** | 2,265,764 kB -> **1,950,692 kB** (-14%) | — |

The baseline grew the young semi on **every** `TestOutOfMemory` run (4/3/2
times); the fixed build grew it **zero** times on every run.

Throughput:
* callgrind (`HashMapOnly 200000 --nojit`, the load-independent meter):
  base 6,881,118,830 / 6,881,195,547 Ir, fix 6,881,345,261 / 6,881,467,511 Ir —
  **+0.004%**, i.e. the clamp arithmetic itself.
* Wall clock: `HashMapOnly` 5.56 s / 6.04 s (base) vs **5.17 s / 5.46 s** (fix) —
  the fixed build is *faster* where expansion used to fire (growing an arena
  means `Vec::resize` zero-filling the whole new capacity under STW).
  `BinTreesClassic 18` 3.61/3.57 s vs 3.73/3.81 s — that workload's `grows` is
  0 in BOTH arms, so it executes an identical code path and the ~4% spread is
  the host's noise floor, which is what it calibrates.

Correctness: `cratonvm-gc --lib` 875 pass / 0 fail (including two new
regression tests, below); `cratonvm-vm --lib` 2313 pass / 0 fail; H2 suite,
30 classes, base vs fix: identical class-for-class.

Near-OOM stability: the `NativeOomProbe` stress (fill the heap with `byte[]`,
then with `ByteBuffer`s, recovering between) run 12x per arm at `--Xmx 1g` —
24/24 clean, no crash in either arm.

## Regression tests (`gc/src/gen_heap.rs`)

* `with_capacity_growth_ceiling_stays_inside_the_xmx_budget` — for `-Xmx` from
  64 KiB to 16 GiB: the recorded budget is `-Xmx`, the initial commit fits it,
  `2 * max_semi + old <= Xmx`, and the ceiling does not shrink the initial semi.
* `with_sizes_keeps_the_historical_unbounded_growth_ceiling` — the explicit-size
  constructors still get `usize::MAX` and `young_semi * MAX_HEAP_EXPANSION_FACTOR`.

## Not fixed here

* **RSS is still larger than `-Xmx`.** The clamp bounds the *Java heap*; the
  remainder is metaspace, JIT code, class metadata and GC-side structures
  (per-arena allocator anchors, card table, mark bitmaps). At `--Xmx 1g` the
  fixed build peaked at ~1.95 GiB RSS with a 1 GiB heap. That non-heap overhead
  is a separate topic.
* **The G1 backend** takes `config.heap_size = total_bytes` and was not audited
  for the same class of over-commit.
