# The `TLAB totals` line's per-thread counters wrap above 65 536 refills (or slow allocations) in one cycle

> **STATUS (2026-10-19, gcj j2/yg2): FIXED IN CODE, PENDING BUILD/PROBE.** The refill and slow-allocation counters are 24 bits wide (exact up to 16 777 215 per thread per cycle) without moving `size_of::<Tlab>()` (272): the low 16 bits stay in `TlabCycleCounters`, bits 16..24 ride in two bytes of `TlabShareSizer` that `samples: u32 -> u16` freed (`gc/src/tlab.rs` `TlabShareSizer::{cycle_refills_hi, cycle_slow_hi}`, `Tlab::{cycle_mark, cycle_refills, cycle_slow_allocations, adopt_cycle_mark_for_refill, keep_on_miss_sized}`, `TlabCycleMark` widened to `u32`, `TLAB_CYCLE_COUNTER_MASK`). The share sizer is carried with the counters at the one refill site (`gc_and_alloc.rs` `tlab_alloc_shaped_inner`: `adopt_share_sizer` before `adopt_cycle_mark_for_refill`). No switch: a diagnostic counter's width. Unit: `cargo test -p cratonvm-gc --lib gcj_j2yg2_a_cycle_of_70000_refills_is_counted` plus the updated `gci_i28tl_marks_difference_across_a_wrap`, `gci_i43_tj_a_hot_cycle_above_32768_refills_is_counted`; layout pin `gci_i28tl_the_cycle_counters_do_not_grow_the_tlab` (272) unchanged. Row: `yg2_tlab_wrap` in `docs/internal/gc-round-gcj-20261019/j2-yg2-report.md`.

> **STATUS (2026-10-19, gcj j1/yg): OPEN (LOW, diagnostic only) -- filed from the residual of `docs/internal/gc/gci-i42ta-tlab-totals-refill-counter-wraps-on-a-hot-thread-RETIRED-UNFIXED-20261018.md`.** No heap-safety impact; the figures of one `-Xlog:gc+tlab=debug` line are wrong for such a thread.

## Evidence (today's tree)

- `gc/src/tlab.rs` `TlabCycleCounters` (~421): `refills: u16`, `slow_allocations: u16`, `refill_waste_words: u32`,
  eight bytes in `Tlab`'s former trailing padding, pinned by `gci_i28tl_the_cycle_counters_do_not_grow_the_tlab`
  (`size_of::<Tlab>() == 272` on x86_64).
- `TlabCycleMark::since` (~465) takes the same-thread difference with `wrapping_sub` at the counters' width, so a
  thread that refills 65 536 + k times between two pauses reports k refills (and `max:` / `waste:` follow).
- Reachable on Generational: a fragmented young (the in-place sweep's) serves mini-TLABs down to
  `FRAG_TLAB_FLOOR`; an eden of 68 MiB in 1 KiB pieces is about 70 000 refills.

## What would close it

Widen `refills` and `slow_allocations` to `u32`: `size_of::<Tlab>()` 272 -> 288 (16-aligned) and `JvmThread` grows by
16 bytes, which `vm/src/runtime/layout_ratchet_tests.rs` asks to be re-frozen in the same commit with a line saying why
the field is hot (it is not: the counters are touched once per refill). Alternatively move them behind the cold tail
(`Option<Box<...>>` per thread, allocated at the first refill). Either is a host-measured layout change, so it was not
made blind.

## How to verify

Unit: a mark pair 70 000 refills apart on one thread reports 70 000. Row: `Gcj1YgTlabTotalsProbe 4096 2` at
`-XX:+UseGenerationalGC -Xmx256m -Xlog:gc+tlab=debug:file=...` with `CRATONVM_DBG_GC_STRESS` off: every
`TLAB totals` line's `refills:` stays within 2x of `allocated bytes / mean TLAB size` for that cycle.
