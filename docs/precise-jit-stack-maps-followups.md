# Shadow-stack precise JIT roots — follow-ups

Status as of 2026-06-05 (merged to dev): the shadow stack **works** — with
`CRATONVM_SHADOW_STACK=1` the moving collector runs under JIT and precisely
relocates JIT-held oops, so **bt10=135854, bt16=14985902, bt18=67674804 are all
golden** and bt18 drains young (~21.5 s). The gate is **default-OFF** and the
default path is byte-identical. Background + the two fixed crash root causes are
in `docs/precise-jit-stack-maps-findings.md`.

These are the remaining items before the mechanism can be trusted broadly and
turned on by default, roughly in priority order.

## 1. OSR-frame *tracking* (correctness, highest priority) — IMPLEMENTED (gated, default-OFF); blocked by a deeper collector bug

**Status (2026-06-05, branch `feat/shadow-stack-followups`):** the mechanism is
built and works mechanically, but it is gated behind its **own** sub-flag
`CRATONVM_SHADOW_OSR_TRACK` (default-OFF) because turning it on **regresses
bt18** via a deeper shadow×conservative-pin GC interaction (see "Why gated"
below). The default `CRATONVM_SHADOW_STACK` path keeps the proven-golden SKIP
behaviour.

**What was done** (the original plan, implemented verbatim):
- `osr_enter` now takes `thread_ptr: i64` (`try_osr` passes
  `thread as *mut JvmThread as i64`; the shadow stack was just `ensure_allocated`d
  by the preceding `set_jit_thread`). Threaded
  `osr_enter → osr_trampoline → emit_osr_trampoline`.
- The cached trampoline now takes a 3rd C-ABI arg (arg2 = R8 / RDX) = `thread_ptr`.
- `CompiledMethod` gained `shadow_savetop_slot_off` + `shadow_off_in_thread`
  (set from the compiler next to `shadow_thread_slot_off`).
- `emit_osr_trampoline`, when `osr_shadow_track_enabled()`, replicates the
  prologue's shadow setup: `MOV [rbp-thread_slot], arg2`; `MOV r11, [arg2+shadow_off]`;
  `MOV [rbp-savetop_slot], r11`. Else (default) it keeps zeroing the thread slot
  (SKIP). Offsets are constant per `target_addr`, so the cache stays valid;
  `thread_ptr` is a runtime arg (different threads OSR the same target), not baked in.

**Validation (8g, the only reliable heap — 16g/30g give a non-golden 68332206
even at baseline, a separate large-heap/promotion confound):**
- `CRATONVM_SHADOW_STACK=1` (OSR track OFF): bt10=135854, bt16=14985902,
  bt18=**67674804** — golden, unchanged. No regression.
- `+ CRATONVM_SHADOW_OSR_TRACK=1`: bt10/16 golden (they do **0** moving GCs — so
  they never actually exercise OSR tracking under a move); bt18 = **68199090**
  (wrong), rc=0 (no crash).

**Why gated (the deeper bug, NOT an OSR-mechanism bug):** `DBG_SHADOW` shows the
track path adds **exactly +1** remapped oop per GC (38→39, 57→58) — the OSR'd
`binaryTrees`'s `longLivedTree` root. Remapping it makes `check()` read
`longLivedTree`'s **to-space** (evacuated) copy instead of the from-space copy
that *every* golden config relies on (SKIP-shadow and `DBG_FORCE_MOVING` both
leave binaryTrees' frame un-remapped). The to-space copy is **structurally wrong**
at bt18's depth-18 scale specifically under the shadow×conservative **mix**:
`FORCE_MOVING` (uniform moving, all-conservative) evacuates the same tree
**correctly** (golden 67674804), so pure moving is fine — the inconsistency is a
copying-collector-with-pinning problem (a moved object whose subtree contains
conservatively-pinned interior nodes), i.e. the same "unresolved interaction"
flagged in `precise-jit-stack-maps-design.md`. It is out of scope for the OSR
trampoline and must be root-caused in the collector (gen_heap evacuation / pin
handling) before OSR tracking — or default-on (§6) — is safe.

**Next for this item:** root-cause the to-space evacuation inconsistency under the
shadow×conservative mix (compare a TRACK-mode GC's to-space `longLivedTree` against
the from-space original; instrument evac of an object that is also conservatively
pinned). Until then `CRATONVM_SHADOW_OSR_TRACK` stays default-OFF.

## 2. Regression pool (correctness, before default-on)

The shadow codegen touches **every** JIT-compiled method's prologue/epilogue and
every GC-capable safepoint, so run the full pool with the gate ON and compare to
golden:
- `test-infra/run-vm-comparison.sh` / the 14-app regression pool with
  `CRATONVM_SHADOW_STACK=1`, looking for checksum/behaviour diffs vs the default
  build (see `reference_cross_vm_comparison_harness`).
- Pay special attention to methods with: many locals (frame-slot offsets >
  disp8), long/double operand entries near safepoints, exception-heavy paths,
  and `invokeinterface`/MIC dispatch (the unpaired spill sites — see §5).

## 3. Exception / deopt unwind safety (correctness)

The watermark `top` restore lives in `emit_epilogue` (normal returns). An
exception unwind or deopt that leaves a JIT frame **without** running its
epilogue won't restore `top` → a transient leak until control returns to the
interpreter boundary.
- **Do:** reset the shadow `top` to a boundary watermark in the
  interpreter→JIT `JitEntryGuard` (save `top` on enter, restore on `Drop`),
  covering all abnormal exits. `set_jit_thread` (vm/src/jit/helpers.rs) is the
  natural place to capture/restore the watermark.
- bintrees has no exceptions on the hot path, so this is untested today.

## 4. Multi-thread shadow scan (correctness for concurrent apps)

`roots.rs` (marking) and `gc.rs` (post-move remap) currently scan/remap only the
**current** thread's shadow stack — matching the existing conservative JIT scan's
single-thread limitation. A STW moving GC in a multi-threaded app must
scan+remap **every** thread's shadow stack.
- **Do:** at the STW safepoint, iterate all threads' `JvmThread.shadow_stack`
  (via the thread registry) for both the marking fold-in and the pointer_map
  remap. Each thread's shadow stack is only valid for ranges pushed by that
  thread (it's per-thread by construction), so this is a straight iteration.

## 5. Push only at balanced safepoints / perf (efficiency)

- The per-safepoint push/reload + prologue/epilogue watermark add instructions
  to every JIT method. Measure overhead: bt18 was ~21.5 s with the gate on vs
  ~31.5 s under `FORCE_MOVING` earlier, but the box was under heavy multi-session
  load — re-measure on a quiet machine against the default build and against
  `FORCE_MOVING`.
- The 3 *unpaired* spill sites (tail-call ~15783, `invokespecial` ~17030, MIC
  ~17656 in `jit/src/x64.rs`) push without an adjacent reload; the per-method
  watermark cleans them, but pushing there is wasted work for sites whose oops
  never need rewriting. Consider gating the push to balanced sites, or skipping
  the push for provably-no-GC callees (e.g. empty `<init>`).

## 6. Default-on + cleanup

- After §1–§4, flip `CRATONVM_SHADOW_STACK` default-on (and decide whether the
  non-moving young sweep / selective-promote paths can be retired).
- Remove the now-unused `emit_zero_local` helper in `jit/src/x64.rs` (dead code,
  harmless), and the debug/bisect toggles (`CRATONVM_DBG_SHADOW2`,
  `CRATONVM_SHADOW_NOPUSH`, `CRATONVM_SHADOW_NORELOAD`) once no longer needed.
- The OSR-entry x64-side note in `x64.rs` (a comment-only no-op where the
  abandoned JIT-side fetch was) can be tidied once §1 lands.

## Quick repro / debug handles
- Run: `CRATONVM_SHADOW_STACK=1 cratonvm … BenchSuite bintrees18` (golden
  67674804). Add `CRATONVM_DBG_SHADOW=1` to see remap activity
  (`[SHADOW] remap: depth=N rewritten=M`).
- Compile-time home dump: `CRATONVM_DBG_SHADOW2=1` →
  `[SHADOW2] push pc=… homes=[…]` per safepoint.
- Bisect: `CRATONVM_SHADOW_NOPUSH=1` (prologue+gate only),
  `CRATONVM_SHADOW_NORELOAD=1` (push only).
- Faulting PCs: hardware-fault handler writes `hs_err_pid<pid>.log`; symbolize
  an RVA offline with `CRATONVM_SYMBOLIZE=<exe+0x…> cratonvm`.
