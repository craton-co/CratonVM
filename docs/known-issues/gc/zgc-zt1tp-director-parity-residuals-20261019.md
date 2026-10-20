# ZGC triggers: HotSpot 25 director parity residuals after zt t1/tp

> **STATUS (2026-10-19, zt t7/x4): item 4 CORRECTED IN CODE, PENDING BUILD.** HotSpot 25.0.3 keeps `-XX:ZAllocationSpikeTolerance=0` and `=-1`; t6 dropped them with a warning. It refuses a malformed value with `Improperly specified VM option`, exit 1; t6 warned. And `=-1` failed the whole clap parse, because the value travelled as a separate `-1` token. On ZGC the launcher now takes HotSpot's double syntax (`hotspot_parse_double_flag`, vm-cli/src/main.rs:7558). The kill is `CRATONVM_ZGC_SPIKE_TOLERANCE_HOTSPOT_PARSE=0`. The director's "unset" word no longer collides with `0.0`. Residuals: `docs/known-issues/gc/zgc-zt7x4-wave-t6-review-residuals-20261019.md` items 1-3 (hex form, the other collectors, `-XX:ZProactive=false`) and 4-5, 7 (the `SoftMaxHeapSize` and warm-up items). Report: `docs/internal/zgc-round-zt-20261018/t7-x4-report.md`.

> **STATUS (2026-10-19, zt t6/tc): OPEN -- items 1, 2, 4, 7 FIXED IN CODE, PENDING BUILD/PROBE; item 3 open; 5, 6 structural.** Item 1: under the opt-in `CRATONVM_ZGC_SOFT_MAX_PACES` the re-arm floor is now a quarter of the SOFT headroom (`collect_garbage`, after the near-full fit; test `zt6tc_the_rearm_floor_is_soft_max_relative_under_the_paces_switch`); the flip itself stays the zt1tp proposal's rows. Item 2: `SoftMaxHeapSize` is a writeable `Long` JMX option on ZGC and `setVMOption` applies it at run time (`native-builtins/src/jmx.rs` `soft_max_flag_row` / `native_flag_set_long_value`, `NativeContext::zgc_soft_max_heap_flag` / `zgc_set_soft_max_heap_flag`, `VmHeap::zgc_set_soft_max_by_management`, `ZgcRealHeap::set_soft_max_heap_bytes_by_management`), opt-in `CRATONVM_ZGC_SOFT_MAX_MANAGEABLE=1`; deciding row `Zt6TcSoftMaxManageable` (stdout SAME as `java -XX:+UseZGC -Xmx1g`, which decides the default origin spelling and the out-of-range exception), positive control: switch off prints `exists=false`. Item 4: `-XX:+/-ZProactive` and `-XX:ZAllocationSpikeTolerance=<f>` parsed into `VmConfig` and the per-heap `ZDirector` (test `zt6tc_zproactive_and_spike_tolerance_reach_the_rules`). Item 7: a `System.gc()` / `GC.run` collection clears the warm-up bit (`CRATONVM_ZGC_WARMUP_EXPLICIT_DOOR`, default ON, `=0` kill; test `zt6tc_an_explicit_door_collection_is_not_a_warmup_step`); an allocation-failure collection still counts (no thread-local mark to read). Item 3 (`Timer`, `-XX:ZCollectionInterval`) is NOT built: it needs a VM-side service that requests a `GcDoor` collection from a non-Java thread, which no current door supports; deciding row as written in item 3. The zs2rv carried test (VM-side conc-start recheck) is not written. Report: `docs/internal/zgc-round-zt-20261018/t6-tc-report.md`.

> **STATUS (2026-10-19, zt t5/pg3): OPEN (items 1-4, 7).** 1: `CRATONVM_ZGC_SOFT_MAX_PACES` flip and a soft-max-relative re-arm floor (`collect_garbage`'s near-full floor block); 2: a writeable `SoftMaxHeapSize` on ZGC (`native_flag_set_value_unsupported`, native-builtins/src/jmx.rs:7384 -> `VmHeap::set_soft_max_heap_bytes`); 3: a `Timer` rule for `-XX:ZCollectionInterval`; 4: parse `-XX:-ZProactive` / `ZAllocationSpikeTolerance`; 7: clear `TRIGGER_BIT_WARMUP` at the explicit doors (row Q6). Carried from zs2rv: a test that drives the VM-side conc-start recheck. Items 5 (no relocation headroom: the STW slide needs no to-space) and 6 (no generational director by default) are structural: GC.md paragraph 'Pacing'.

> **STATUS (2026-10-19, zt t3/tp): OPEN, unchanged in code.** Item 7's warm-up pre-emption by the allocation-budget clause (zs2rv item 1) has the same owner and no fix this wave. The budget engages only after a pause over 200 ms, and standing it down during warm-up would lengthen exactly those pauses. Deciding row: Q6 in `docs/internal/zgc-round-zt-20261018/t3-tp-report.md`.**

> **STATUS (2026-10-19, zt t1/tp): OPEN.** Found by reading on `zt1-tp` (base `2dc18ab9a0`). None is a correctness defect. Each one is a difference from `java -XX:+UseZGC` on JDK 25 in when, or how often, a collection runs. The fixes this lane made are in `docs/internal/zgc-round-zt-20261018/t1-tp-report.md`.

## 1. `-XX:SoftMaxHeapSize` only moves the warm-up points on the default arm

HotSpot's director reads the soft maximum in every rule:
- warm-up (10/20/30 % of it);
- allocation rate (free space = soft max - used);
- high usage (5 % of it free);
- proactive (10 % of it of growth).

On HotSpot, `-Xmx1g -XX:SoftMaxHeapSize=256m` keeps the heap near 256 MiB, and the heap grows past that only when the live set needs it.

This VM reads the soft maximum in two places:
- the warm-up clause of `needs_gc` (`gc/src/zgc.rs:28645`, the `soft_max_capacity() / 10` step);
- the director, which is opt-in (`CRATONVM_ZGC_RATE_TRIGGER=1`).

After the three warm-up cycles, the same flags collect at 75 % of `-Xmx` (768 MiB).

- **Built (opt-in):** `CRATONVM_ZGC_SOFT_MAX_PACES=1`. With it, the live-bytes clause also fires at 75 % of the soft maximum (`ZgcRealHeap::pacing_threshold`, `gc/src/zgc.rs:17385`). Probe: `tools/probes/Zt1TpSoftMaxPaces.java`. The flip proposal is `docs/internal/proposals/gc/zgc-zt1tp-proposal-soft-max-paces-the-default-trigger-20261019.md`.
- **Still open:** the re-arm floor (`live + free / 4`) is a share of the CAPACITY, not of the soft maximum. With the switch on, the heap therefore still fills to roughly `live + (Xmx - live) / 4` above the soft threshold. Making the floor soft-max relative means editing the near-full floor block of `collect_garbage`, which lane `sp` owns.

## 2. `SoftMaxHeapSize` cannot be changed at run time

On HotSpot, `SoftMaxHeapSize` is a manageable flag, and ZGC re-reads it on every director tick. Both of these change it:
- `HotSpotDiagnosticMXBean.setVMOption("SoftMaxHeapSize", ...)`;
- `jcmd VM.set_flag`.

Here every flag write is refused with `VM Option "<name>" is not writeable` (`native-builtins/src/jmx.rs:7084`, `native_flag_set_value_unsupported`). `ZgcRealHeap::set_soft_max_heap_bytes` can already be called at any time (it is two relaxed stores).

**What closes it:** a writeable-flag row for `SoftMaxHeapSize`, on ZGC only, that calls `VmHeap::set_soft_max_heap_bytes`. This is lane `ob`'s file (`jmx.rs`).

## 3. No `Timer` rule: `-XX:ZCollectionInterval` (and `...Minor` / `...Major`) is ignored without a word

HotSpot collects every N seconds under `-XX:ZCollectionInterval=N` (cause `Timer`), and it does so even in an idle program. The launcher drops every unimplemented `-XX:` flag silently (`vm-cli/src/main.rs:4635`; only `CRATONVM_DBG_ARGS` names it).

Polling `needs_gc` cannot implement this rule, because an idle program does not poll. It needs a timer on the VM side, such as the uncommitter thread (`vm_init.rs`, the `ZUncommitDelay` service), that requests a `GcDoor` collection with cause `Timer`.

**What closes it:**
- a timer service that requests the collection;
- a `Timer` cause spelling in `hotspot_cause_for_mask`;
- a row `-XX:ZCollectionInterval=1` on an idle probe, which should show one `Timer` line per second as on HotSpot.

## 4. `-XX:-ZProactive` and `-XX:ZAllocationSpikeTolerance` are not read

The director hard-codes:
- the proactive rule (always on);
- `ALLOCATION_SPIKE_TOLERANCE = 2.0` (`gc/src/zgc/director.rs`).

Both flags are dropped silently. This only matters under the opt-in director.

**What closes it:** parse both into `VmConfig`, then into `ZDirector` setters (per VM).

## 5. HotSpot's relocation headroom is not subtracted from the director's free space

HotSpot's rules compute their free space as follows:
1. Start from `soft_max - used`.
2. Subtract `ZHeuristics::relocation_headroom()`, the pages its concurrent relocation needs so that it does not fall back to in-place relocation.

This collector relocates by a stop-the-world slide, which needs no to-space, so the subtraction is left out on purpose. This entry records the difference. Do not "fix" it without a concurrent relocator.

## 6. JDK 25's generational director is not modelled

`gc/src/zgc/director.rs` carries the non-generational director (JDK 17-22). JDK 25 splits its decisions:
- minor rules: `rule_minor_allocation_rate`, `rule_minor_high_usage`, `rule_minor_timer`;
- a cost-weighted major rule: `rule_major_allocation_rate`, which compares the young and old GC costs.

So HotSpot 25's `-Xlog:gc` shows minor `Allocation Rate` cycles between the major ones. That has no counterpart until this VM's young generation is on by default. The cause *names* already match.

## 7. The warm-up door race (zs1tp residuals item 5) is unchanged

A collection that a door other than the poll takes (an allocation failure, or a racing `System.gc()`) still counts as a warm-up step whenever the warm-up bit is on the mask. The gc crate cannot see the door. Since zt, the same mask also feeds the director's warmth (`note_warmup_cycle`, `gc/src/zgc.rs:27975`), so the two counts at least agree.

**What closes it:** clear `TRIGGER_BIT_WARMUP` from `trigger_seen` at the explicit doors (`run_collection_pause` with `GcDoor::SystemGc` / `DiagnosticCommand` / ...), before the collection. That needs a `VmHeap` hook and is a cross-crate change.
