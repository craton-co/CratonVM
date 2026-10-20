# ZGC: small allocations crawl per object while a collection is due and large arrays keep the heap past the trigger

> **STATUS (2026-10-19, zt t5/pg3): OPEN (a flip decision with local rows).** Run CH-3 / CH-4 (`docs/internal/zgc-round-zt-20261018/t4-ch-report.md`) with T9 of the t1-tp report on the Windows box; flip `CRATONVM_ZGC_REFILL_GATE_COLLECTS` (`zgc_refill_gate_collect`, vm/src/runtime/interpreter/gc_and_alloc.rs:10530) and `CRATONVM_ZGC_JIT_NEWARRAY_HEADROOM` (`zgc_jit_newarray_headroom_enabled`, :8918; zr3ja retired into this page) together if the rule holds. Item 4 (stop-the-world collections against HotSpot's concurrent cycles) is structural (GC.md 'Pacing').

> **STATUS (2026-10-19, zt t4/ch): OPEN, opt-in fix in code, PENDING BUILD/PROBE.
> `CRATONVM_ZGC_REFILL_GATE_COLLECTS=1` takes the due collection at the refused refill; the window
> is now counted on every run (`gate_refused=` on `[GC] zgc-refusal:`). Rows CH-3 / CH-4 in
> `docs/internal/zgc-round-zt-20261018/t4-ch-report.md` decide the flip, together with the
> existing opt-in `CRATONVM_ZGC_JIT_NEWARRAY_HEADROOM` (t3/tp row T9).**

## Local evidence (orchestrator, Windows, HotSpot 25.0.3 `-XX:+UseZGC`, 10 s windows, `-Xmx512m`)

- `Zr2A2SmallUnderLarge 4 4 10 1024 prim`: CVM `small_allocs` 47-81 M against HotSpot 550-705 M
  (8-10x fewer); `large_ops` 19-26 k against 43-55 k at similar `large_ops_per_cpu_s`.
- `Zr1AlLargeAllocContention 4 4 10 1024`: `large_ops` 24-43 k against 50-59 k; small ops
  several-x fewer.

## Mechanism (by reading, `zt4-ch` tree)

1. **On ZGC the compiled refill gate is the occupancy verdict.** `tlab_alloc_shaped_inner`
   (`vm/src/runtime/interpreter/gc_and_alloc.rs`) refuses a refill when
   `!young_bump_headroom(..) && !young_has_free_block(..)`; on ZGC both are `!needs_gc()`
   (`gc/src/vm_heap.rs` `young_bump_headroom` / `young_has_free_block`, the `Zgc` arms). So a
   refusal means exactly "a collection is due".
2. **The due collection waits.** It is taken by the refill trigger's consult, which runs when
   `refill_bytes_since_gc >= 4 MiB` (successful refills only, and a refused gate makes none) or
   `slowpath_entries_since_gc >= 65,536`; or by the wedge breaker after 16,384 consecutive
   refusals (`tlab_refill_wedge_break`, `WEDGE_BREAK_THRESHOLD`, plus 64 MiB of progress since its
   previous break). Until then the refused thread returns `None` and every object takes the
   per-object locked path (`try_alloc_*_full` -> `ZgcRealHeap::alloc_raw`: the arena lock, a
   registry `fetch_or`, `gc_quiescence::note_allocated`, a shared `fetch_add`), each also paying
   the helper round trip.
3. **Large arrays never ask.** `jit_newarray_body` (`vm/src/jit/helpers.rs`) for an array above the
   TLAB cap goes `young_bump_headroom` (false once due) -> `try_alloc_array_full` (succeeds while
   the arena has room). Its ZGC "no headroom" collection is the opt-in
   `CRATONVM_ZGC_JIT_NEWARRAY_HEADROOM`. So with four threads streaming 1 MiB arrays at ~2.5 GB/s
   into a 512 MiB heap, `allocated` passes the trigger every ~0.15 s and the heap is driven on
   toward the arena's end by the large threads, while the small threads spill per object and
   contend with them for the arena lock, until the 16,384th refusal or an arena failure collects.
   HotSpot starts a concurrent cycle on its allocation-rate rule and its mutators keep
   allocating from pages throughout.
4. **Every collection here is stop-the-world** (`Z_CONC_START_PERCENT_DEFAULT = 0`): at ~70
   collections per 10 s window the small threads also lose the pauses (`gc_time_ms` on the
   probe's own line says how much).

## Fix in code (opt-in) and what would close the page

- `CRATONVM_ZGC_REFILL_GATE_COLLECTS=1` (`zgc_refill_gate_collect`): the refused refill takes the
  due collection at once through the stamped occupancy door (`tlab-alloc-shaped`, the consult's own
  door and label), rate-limited to one per 1/32 of the heap allocated since the last collection
  (`newarray_headroom_rate_skips`, the storm guard), then re-probes the gate.
- With `CRATONVM_ZGC_JIT_NEWARRAY_HEADROOM=1` the large threads also collect on the verdict.
- Close when row CH-3 shows the combined arm at >= 2x the default's `small_allocs` with
  `large_ops` not lower, `gate_refused` falling by >= 10x, stdout SAME on the soundness rows,
  and CPU <= 1.05x on the regression rows; then flip both (default ON, `=0` kill).
- What remains after that is item 4 (STW collections against a concurrent HotSpot), which is the
  concurrent-cycle question (`CRATONVM_ZGC_CONC_EVERY_TRIGGER`), not a trigger fix.
