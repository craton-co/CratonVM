# ZGC residuals handed over by the gci round (ZGC owner; out of the gci round's scope)

> **STATUS (2026-10-19, zt t6/rv3): item 2 FIXED IN CODE (`ac8bdd71fb`), PENDING BUILD.** Every named reader reads the value through `runtime_flag_on` (`CRATONVM_ZGC_NO_JIT_READ_BOUNDS` as `!runtime_flag_on`, default ON kept); latched readers got an uncached `_requested` half; the vm `[ZGC-WILD-SECTIONS]` reader calls `cratonvm_gc::zgc::dbg_zgc_wild()` (one latch); four more ZGC-owned presence readers converted too (`DBG_ZGC_RELOCATE_PHASES`, `DBG_ZGC_SLIDE_PANIC`, `DBG_ZGC_REWRITE`, `DBG_ZGC_SATB_WINDOW`); the `ZGC_PAGE_SURVEY` test guards read the value. Tests (in `gc/src/zgc.rs` rather than `types`, so they drive the readers themselves): `zt6rv3_opt_in_zgc_switches_read_the_value_not_presence` (off words off, on words on = positive control, unset off), `zt6rv3_no_jit_read_bounds_reads_the_value`, and the source ratchet `zt6rv3_no_presence_reader_of_these_names_is_left`. Left alone, by design: `CRATONVM_ZGC_CONC_START` (presence of any value decides the every-trigger default), `zgc_sweep_skip_dead_headers_explicit` (prints only when set), the `ZGC_MARK_ROOT_FILTER_CONC` test guard. Left alone as shared / non-ZGC presence readers (list for the owners): gc_and_alloc.rs `DBG_MTROOTS`, `GC_NO_FRAME_TRACE_SPAN_RETIRE`, `DBG_STW_CENSUS`, `DBG_STW_NATIVE_RING`, `DBG_REMAP_TRACE`, `DBG_STW_EXPECTED_IDS`, `DBG_CORRUPT_FRAMES`, `DBG_GC_OVERHEAD`, `DBG_OVERLAY_PRUNE`, `DBG_TLABMISS` (x2), `DBG_ROOTSNAP`, `DBG_ROOTSNAP_VERIFY`, `DBG_REFPROC_REMARK` (x2); gc/src/vm_heap.rs `GC_STATS`. Item 1 and 3 FIXED (above); the page can retire once the build is green.

> **STATUS (2026-10-19, zt t5/pg3): OPEN (item 2 only).** Item 1 is FIXED on today's tree: `jit_invoke_virtual_mic_body` (vm/src/jit/helpers.rs, the `args_forwarded` block after `forward_jit_reference_args`) roots a forwarded copy through `CompileArgPins` across every arm (jr20 wave 9, lane gcE). Item 3 FIXED (2026-10-18). Item 2 is still open, every reader still `runtime_var_os(..).is_some()`/`is_none()`: gc/src/zgc.rs `CRATONVM_DBG_ZGC_WILD` (:26148, and vm/src/runtime/interpreter/gc_and_alloc.rs:5478), `DBG_FINCAND` (:26131), `DBG_ZGC_CORPSE` (:24572), `DBG_ZGC_HIGH` (:12924), `DBG_ZGC_TARGET` (:16614, :16655), `DBG_ZGC_VERIFY_SLIDE` (:24681), `ZGC_ASSUME_REWRITABLE` (:13451), `ZGC_JIT_BLANKET_REFUSAL` (:22967), `ZGC_TARGETED_COMPACTION` (:23056), `ZGC_TLAB_FLAG_PUBLISH_FALSE` (:3534), `ZGC_UNREWRITABLE_PEER_REFUSES` (:21084), `ZGC_NO_JIT_READ_BOUNDS` (:831, `is_none`). `ZGC_PAGE_SURVEY`'s production reader is `runtime_flag_on` now (gc/src/zgc/page.rs:1568); only its tests read raw. Close: `runtime_flag_on` at each reader plus a `types` test per name that `=0` reads off.

> **STATUS (2026-10-18): Items 1-2 remain OPEN; item 3 is FIXED AND VERIFIED.** The LRU retirement below covers both modes, both engines and the concurrent residual. The other two items describe separate issues.

> **Historical status (gci orchestrator, 2026-10-14): OPEN, ZGC-only.** Collected when the gci round retired three pages whose
> every other item was closed. The gci round (Generational + G1) did not touch ZGC; nothing here affects a
> Generational or G1 run. Retire each item when a ZGC round converts or refutes it.

## 1. A MIC door that read a forwarded Rust copy of its arguments re-runs the callee from it (OPEN, LOW)

From `docs/internal/gc/gch-h1jp-dispatch-doors-and-oome-ladder-review-findings-DONE-20261012.md` item 2.

- **Evidence.** `jit_invoke_virtual_mic_body` (`vm/src/jit/helpers.rs`): `let forwarded_args =
  forward_jit_reference_args(..); let args_slice = forwarded_args.as_deref().unwrap_or(args_slice);`. When the
  SATB flush forwards a word (a moved object the caller's block still names; ZGC's lazy remap is the producer),
  every later arm (hit, PIC and megamorphic-table calls and their sentinel service) runs on that `Vec`, which is in
  no map and no pin list. A re-run from entry after a moving collection inside the callee reads its pre-move words.
- **Why not Generational / G1.** Both move objects only inside a stop-the-world pause that rewrites every rooted
  word, the compiled caller's block included, so `forward_jit_reference_args` finds no forwarded word there (gci
  i1/ob and i5g/g1f readings).
- **Fix.** Pin the forwarded copy (`CompileArgPins::pin_raw`) across the call when `forwarded_args.is_some()` and
  the callee is not an owning body. Build it with a ZGC row that shows a re-run on a stale word.

## 2. ZGC flag readers that still parse presence, not value (OPEN, LOW, debug/opt-in names only)

From `docs/internal/gc/gci-i3fl-gc-typed-flag-fields-and-shared-readers-still-parse-presence-CLOSED-20261014.md` item 3
(its Generational and G1 halves are FIXED). Each name is unset by default, so no default run is affected; `=0`
turns these on.

- `CRATONVM_DBG_ZGC_WILD`: `gc/src/zgc.rs` plus `vm/src/runtime/interpreter/gc_and_alloc.rs`; convert both together.
- Presence, single reader: `DBG_FINCAND`, `DBG_ZGC_CORPSE`, `DBG_ZGC_HIGH`, `DBG_ZGC_TARGET` (x2),
  `DBG_ZGC_VERIFY_SLIDE`, `ZGC_ASSUME_REWRITABLE`, `ZGC_JIT_BLANKET_REFUSAL`, `ZGC_TARGETED_COMPACTION`,
  `ZGC_TLAB_FLAG_PUBLISH_FALSE`, `ZGC_UNREWRITABLE_PEER_REFUSES`, `ZGC_NO_JIT_READ_BOUNDS` (`is_none`).
- `ZGC_PAGE_SURVEY`: `gc/src/zgc/page.rs` has a raw `var_os().is_none()` read beside a `runtime_flag_on` reader;
  the two disagree on `=0`.
- **Fix.** `runtime_flag_on` (or `!runtime_flag_on` for an opt-out) at each reader; a `types` unit test per name
  that `=0` reads off.

## 3. ZGC's SoftReference LRU window is not HotSpot's (FIXED AND VERIFIED 2026-10-18)

Closed by fresh HotSpot 25 parity and concurrent soundness gates; see
`docs/internal/gc/gch-h2jr-soft-lru-window-on-g1-and-zgc-diverges-from-hotspot-FIXED-20261012.md`
and `docs/internal/gc/zgc-zr1rf-concurrent-mark-traces-reference-slots-strongly-FIXED-20261018.md`.
The runtime already used `ReferenceProcessor::new_for_backend` with ZGC's
HotSpot default. Concurrent reference handoff is now default ON; false
root-filter removals and the shared queue delivery race found by repeated
acceptance were also fixed. The evidence and proposed fix below are historical.

From `docs/internal/gc/gch-h2jr-soft-lru-window-on-g1-and-zgc-diverges-from-hotspot-FIXED-20261012.md` (its G1 half
is FIXED: `CRATONVM_SOFTREF_HOTSPOT_LRU` default ON on Generational and G1).

- **Evidence.** `ak1_softlru_zgc` prints `false` where HotSpot 25 prints `true`: ZGC clears a soft reference
  that was read within the last few milliseconds. `ak1_softidle_zgc` is SAME, so the free-heap input
  (`VmHeap::soft_ref_policy_free_mb`'s ZGC arm) is HotSpot's; the clock or the per-cycle timestamp
  (`publish_soft_reference_clock`, end of `process_references_after_gc`) is what differs on ZGC.
- **Fix.** Build ZGC's reference processor with HotSpot's inputs, as `vm_init.rs`
  `soft_lru_takes_hotspot_inputs_by_default` does for Generational and G1; decide on the `ak1_softlru_zgc` row.
