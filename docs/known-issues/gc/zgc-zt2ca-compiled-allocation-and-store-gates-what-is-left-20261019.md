# ZGC compiled allocation and store gates: what is left after zr and zs (one page for nine)

> **STATUS (2026-10-19, zt t6/tc): OPEN.** Item 7 FIXED IN PROBES, PENDING HOTSPOT REFERENCES: `Zs2Ca2AllocWindows` and `Zs7VlVarlenParts` print a `recheck` line after the timed windows, over 2000 rounds x every shape through the compiled kernels (`... recheck shapes=6 ops=768000 bad=0 ok=true`); their HotSpot reference outputs must be regenerated. The spill-sink proposal (`zgc-zt4ca2-proposal-sink-the-blind-spill-at-single-pass-zgc-new-20261019.md`) is NOT built: its step-4 precondition holds by reading (frozen peers are scanned for registers, machine stack and shadow stack, `vm/src/jit/conservative_roots.rs` :483-:501), but its steps 2-3 (a per-site clobber set and a second deferred-spill edge with its own oop map) could not be shown root-complete without a build, so the proposal's rows still decide. Items 1, 3, 3b, 4, 5, section 2's flips, item 9 (row H3) and the IR fast-join lowering test are unchanged: each is decided by the row named in its item.

> **STATUS (2026-10-19, zt t5/pg3): OPEN.** Section 2's opt-in flips (Windows rows `t4ca2-ab.ps1`), item 1's annotate row, items 3 / 3b / 4 / 5, item 7's probe check lines, item 9 (row H3, Windows form) and the zs5bt2 proposal remain ZGC JIT-emitter work. Carried from zs2rv: a lowering-level test of the IR fast-join gate (`pending_shadow.is_empty()`). Item 10 lives on the JIT owners' churn page.

> **STATUS (2026-10-19, zt t4/ca2): OPEN.** Item 3 corrected (the single-pass tier is a hot tier for one-activation loops the IR tier refuses) and item 3b added (zr11ia P2 relabelled: the `toString()` pair's pre-check in `objects.rs` and its `concat_emit.rs` mirror). Items 9 (row H3, with a Windows form) and 10 (frame homes / I1, moved to the JIT owners' page) carried from the retired pages. The section-2 keys now have Windows-runnable deciding rows: `docs/internal/zgc-round-zt-20261018/t4ca2-ab.ps1` and section 5 of `t4-ca2-report.md`; note there that the fixed-duration probes' process CPU cannot decide anything.

> **STATUS (2026-10-19, zt t2/ca): OPEN.** This page replaces the compiled-allocation pages zs1ca, zs2ca2, zs6ca3 (varlen), zr7ca (five times), zr11ia and zr8zb, and the store-gate page zs3bt. Each of those now carries a STATUS line pointing here and can retire. What they had open is below, re-checked on `zt2-ca` (base `67bc580ab8`). Two cuts are new this wave, both opt-in and value-identical (`CRATONVM_JIT_ZGC_NEWARRAY_SIZE_OFF_CHAIN`, `CRATONVM_JIT_ZGC_ALLOC_PREFETCH_INSTANCE_LINES`). The deciding rows are in `docs/internal/zgc-round-zt-20261018/t2-ca-report.md` and `t2ca-ab.spec`.

## 1. What the host has decided (zs orch-notes; nothing re-measured since)

| key | state | evidence |
|---|---|---|
| `CRATONVM_JIT_ZGC_INLINE_ARM_FAST_JOIN`, `CRATONVM_JIT_IR_NEWARRAY_CONST_LENGTH`, `CRATONVM_JIT_ZGC_ANNOUNCE_SKIP_SPAN_TEST` (zs s1) | ON | ab-s1bf `fixed0` D 6.22 vs all three off 7.98 ns; ab-s2b A4 (all cuts off) 3.17 vs A1 2.81 ns |
| `CRATONVM_JIT_ZGC_TLAB_ALIGN_TEST` (zs s6) | ON | ab-s6a cawin D 21.81 vs `=0` 21.99 s CPU |
| `CRATONVM_JIT_ZGC_FUSED_REF_STORE_GATE`, `CRATONVM_ZGC_JIT_POST_GATE_TRUTH` (zs s3) | ON | ab-s3a btwin 21.76 vs gate off 22.73 s; btc 20.74 vs 20.78 s |
| `CRATONVM_JIT_ZGC_FUSED_GATE_ALL_SITES` (zs s5) | ON | ab-s5a bt2 cost D 21.74 vs `=0` 21.83 s; s7a SP-0 `sp=4 sp_k=0 ir_k=1`, `spa=8 spa_k=0` (the single-pass arm engaged), SP-3 snap SAME both modes |
| `CRATONVM_JIT_IR_ZGC_ARRAY_HEADER_QWORDS` (zs s2) | opt-in | lost on s2b (+5.8 % CPU), a tie on s4b (21.51 vs 21.42 s) |
| `CRATONVM_ZGC_JIT_ANNOUNCE_EPOCH_GATE` (zs s2) | opt-in | flat three times: s2b A3, s6a 21.97 vs D 21.81 s |
| `CRATONVM_JIT_ZGC_FRESH_RECEIVER_STORE` (zs s5) | opt-in | ab-s5a 21.55 vs D 21.74 s, inside noise |
| `CRATONVM_ZGC_JIT_SHORT_ANNOUNCE` (zr z9) | opt-in | zr xf, no flip |
| varlen as a whole | parity | s7a V4 `Zs7VlVarlenParts 20`: CVM 22.06 s vs HotSpot 22.30 s CPU |

What the numbers say about the model: a five-instruction cut that stays off the cursor chain (the epoch gate) was flat, while a two-instruction cut that took work off the chain (the align test) won. So the s2/ca2 model ("0.05 ns per instruction, throughput-bound") does not describe the host core. The latency of the one value the loop carries, the TLAB cursor (load -> bump -> commit through memory), is what moved. The new `SIZE_OFF_CHAIN` cut is built on that reading (section 3).

## 2. Opt-in keys with no host row yet (zt triage t1)

| key | built by | soundness (re-read on `zt2-ca`) | rows |
|---|---|---|---|
| `CRATONVM_JIT_ZGC_INLINE_ARM_FALL_THROUGH` | zs s9/ca4 | Sound by reading: each half is the unsplit bytes; the cold tail runs only from the fast half's own branches and returns to the continuation before `store_rax`; the exit row is the plain call-exception row (admission excludes protected bcis, armed call-throw plans and catch arms); `pending_shadow` is checked after the site's map, so the skipped reload would emit nothing. One defect FIXED this wave: the split ignored `CRATONVM_JIT_ZGC_INLINE_ARM_SKIPS_REPUBLISH=0` (`ir_lower.rs` `zgc_alloc_tail_admitted`, :26653). | t2ca-ab FT arms; S rows of the report |
| `CRATONVM_JIT_ZGC_NEWARRAY_SKIP_WORD_ROUND` | zs s10/ca5 | Sound: `n << 3` is on the grid; the length is screened to the cap first. Superseded by `SIZE_OFF_CHAIN` (which drops more on the same sites); keep only if SKR beats OC. | SKR arms |
| `CRATONVM_JIT_ZGC_ALLOC_PREFETCH` / `_DISTANCE` | zs s10/ca5 | Sound: `0F 0D /1` after the passed end test reads RAX only, never faults, is a NOP on CPUs without PREFETCHW, touches no flag a later instruction reads. | PF arms, now with the per-instance count |
| `CRATONVM_JIT_ZGC_NEWARRAY_SIZE_OFF_CHAIN` | zt t2/ca | Executed differential over every element size (`runtime_lowering.rs` `zt2ca_*`, :3407, :3456). | OC arms |
| `CRATONVM_JIT_ZGC_ALLOC_PREFETCH_INSTANCE_LINES` | zt t2/ca | As the prefetch. | PF3I1 / PF3I0 arms |

## 3. Still open

1. **The announce is ~24 instructions per compiled allocation that HotSpot does not run.** No direction is left that the host has not refuted:
   - proposal C, a bit-less live TLAB: refuted by the zr z8a census (1-2 k header reads per peer query);
   - the per-buffer gate: not worth building (zs s5/ta);
   - aligned chunks with a biased bit address: refuted with the epoch gate (s10/pg2);
   - the epoch gate itself: flat.

   If the latency model of section 1 holds, the announce's instructions are not on the cursor chain and cost little more than their issue slots. **Deciding row (one run):** `perf annotate` of `burstFixed0` with `CRATONVM_JIT_PERF_MAP=1` (row I2 of the zs2ca2 page, never run). If the samples sit on the cursor load and the commit, the announce is not the gap and this item closes.
2. **The run-time-length bump's three-component `LEA` sat on the cursor chain.** Built opt-in this wave: `CRATONVM_JIT_ZGC_NEWARRAY_SIZE_OFF_CHAIN` (`runtime_lowering.rs` `emit_newarray_footprint_off_chain`, :1587). Rows OC.
3. **The single-pass tier has none of the IR tier's cuts** (constant length, align test, fall-through). It was read as "runs before tier-up only", which is wrong for one hot shape: a one-activation loop in a method the IR tier refuses (an `invokedynamic` string concat after the loop) runs its whole life in the single-pass OSR body. `G1ChurnPauseProbe` is that shape (zt t4/ca2, item 10 below and `churn-loop-runs-in-the-single-pass-osr-body-20261019.md`). The census that decides whether the cuts are worth porting is row CA2-3 of `docs/internal/zgc-round-zt-20261018/t4-ca2-report.md`.
3b. **zr11ia P2 (corrected, zt t4/ca2; t3/x2 noted the old wording mislabelled it).** P2 is NOT the single-pass `new`/`newarray` sequence. It is the long form of the start-bit PRE-CHECK of the call-free pair that the `StringBuilder.toString()` doors emit, which per object re-derives the granule from `base` / `span` instead of the owned-word window. The pre-check exists twice, byte for byte:
   - `jit/src/x64/objects.rs` `Compiler::emit_zgc_start_bit_precheck_form` (:4074), called by the single-pass door in `x64/op_invoke.rs` (:18682);
   - its free-function mirror `jit/src/x64/concat_emit.rs` `emit_zgc_start_bit_precheck_form` (:1241; callers :1087 and through `emit_zgc_start_bit_precheck` :1225 at :1557, :1680, :1891, :2309), pinned equal to the first by the test at `x64/objects.rs` :8876-:8882.

   A trim must change both and keep that byte-identity test. The long form is the default (the short form, `CRATONVM_ZGC_JIT_SHORT_ANNOUNCE`, is opt-in), so P2 is live: zr z11/ia's window trim took the same granule arithmetic in the allocation sequence from 11 instructions (3 loads) to two dword compares, and P2 would save about that per object of a `toString()` pair. Not built; it needs a census of `toString()` door executions in a hot row before it is worth a switch.
4. **Slow-path redundancies, not built (helper arm only):**
   - `emit_zgc_commit_after_helper` (`x64/objects.rs` :552) re-aligns the cursor (`ADD R11, 7; AND R11, -8`, :560), which the align test has already proven aligned on ZGC;
   - `emit_zgc_object_end_into_rcx` (:569) rounds an 8-byte element's data size (:600).
5. **The fall-through split is not offered at gcDead-armed array sites** (`ir_lower.rs` :33291 `!self.alloc_site_armed(id)`). Their slow path carries held clears. A split would need those clears in the cold half.
6. **The varlen residual is not the JIT's.** s7a V4 shows parity with HotSpot. What remains of `varlen`'s per-byte cost is the refill's carve zeroing, owned by lane `al` (`zgc-zr7ca-proposal-chunk-zeroing-frontier-*`).
7. **Two allocation probes check their kernels only before warm-up.**
   - `Zs2Ca2AllocWindows` (:189) and `Zs7VlVarlenParts` (:193) run their `check` line over the first 200 bursts in `main`, before the timed windows. So a green check line does not show that the compiled kernel is sound: "warm in drive, assert in main" (memory: that measures the interpreter).
   - The soundness rows of zs s9/s10 that leaned on those check lines are weaker than they read.
   - `Zs1CaAllocParts` has the same shape: its phase-1 check runs over the first 200 rounds by default. But its third argument raises the count: `Zs1CaAllocParts <shape> 5 20000` checks 20,000 rounds, most of them compiled. The report's soundness rows use that form.
   - `Zt2CaArraySizes` (new) checks inside the hot bursts. `Zs9Ca4ColdExits mix` checks its whole hot loop.
8. **Store gates (zs5bt2 proposal):** Q1 (fresh census), Q2 (loop-held word), Q3 (fresh `aastore`) and Q4 (single-pass null-old test under a mark) are not built. That proposal page stays open and carries them.

9. **Row H3, carried from the retired zr7ca / zr8zb pages (zt t3/x2 audit; zt t4/ca2).** What share of the allocation rows' CPU is the collector, not the allocation path, and what loop is the `collect_garbage` self time (4.55 % of `Zr3JaRefArrayAlloc prim` on the zr host; by reading, `relocated_this_cycle`'s `registry.snapshot_within(..).bases()` at `gc/src/zgc.rs` :32473-:32479 is the leading candidate, then the post-sweep span install). Two forms:
   - **Linux (the original):** `CRATONVM_JIT_PERF_MAP=1 sudo -n perf record -e cpu-clock -g -o zr7ca.data cratonvm -Xmx512m -cp $P Zr3JaRefArrayAlloc 1 10 64 prim 400`, then `perf annotate -s ZgcRealHeap::collect_garbage` and `-s ZgcRealHeap::sweep_bitmap_range`.
   - **Windows (the orchestrator's box; no `perf`):** the share needs no profiler. `CRATONVM_GC_STATS=1 cratonvm --verbose:gc -Xmx512m -Xlog:gc:stderr -cp $P Zr3JaRefArrayAlloc 1 10 64 prim 400 2>h3.err`: sum `total_us` of the `[GC] zgc-pause:` lines and the `pause_with_relocate_us` of the relocation lines, (both lines print only with ZGC's GC log on, `ZgcRealHeap::gc_log_enabled`, `gc/src/zgc.rs` :16455; a run without them is vacuous), and divide by the process CPU (`TotalProcessorTime`, as in `t4ca2-ab.ps1`). Collector share = sum / CPU; the probe's `ns_per_alloc_cpu` times that share is the collector's ns per allocation, against HotSpot's whole 10.5 ns. Naming the self loop needs a sampler: the release profile keeps function symbols in the PDB (`Cargo.toml` `[profile.release]` `strip = "none"`), so `wpr -start CPU` / `wpr -stop h3.etl` (elevated) and WPA's "CPU Usage (Sampled)" by function on `cratonvm.exe` name it. Positive control: `sweep_bitmap_range` must appear in the sampled stacks (it was 19.5 % on the zr host); if it does not, the symbols did not load.
   - Closes when the share is recorded: under ~15 % of the row's CPU, the allocation rows' gap is the mutator path and H3 is closed; above, the named loop gets a page in the collector's area.
10. **The IR tier's frame homes and row I1 (carried from the retired zs2ca2 page).** About 40 instructions per iteration of `burstFixed0` were not allocation: the index and induction homed to frame slots, the `aastore` checks and the barrier gates, the poll. They are the JIT tier's (register allocation), not ZGC's, and now live on `churn-loop-runs-in-the-single-pass-osr-body-20261019.md` with the churn loop's evidence. Row I1 (Windows-runnable as written: `CRATONVM_DBG_JIT_DISASM=Zs2Ca2AllocWindows.burstFixed0 cratonvm -Xmx2g -cp $P Zs2Ca2AllocWindows 3 2>fixed0.disasm`, then count the fast-path instructions from the loop head to the back edge) is the deciding row there.

## 4. What would close this page

- Section 2's flips decided on the host.
- Item 1's annotate row run, and the item closed or re-scoped.
- Items 3-5 either built or declared not worth it with a census.
- Item 7 decided: new check lines in those two probes (with HotSpot references regenerated), or the probes documented as timing-only.
