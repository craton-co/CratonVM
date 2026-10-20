# The in-native band narrowing never engages (`dep_band_narrowed=0`)

> **STATUS (2026-10-19, gcj j2/jn2): OPEN, instrumented -- the refusal is now counted by its reason; the deciding run is block NB2 of `docs/internal/gc-round-gcj-20261019/j2-jn2-report.md`.** No behaviour change (`CRATONVM_JNI_IN_NATIVE_BAND_NARROW` stays opt-in; KEEP OFF per the j1 verdict).

*Filed 2026-10-19 by gcj j2/jn2, from the j1 build's NB rows (orch-notes: `dep_band_narrow_probe=2605`
but `dep_band_narrowed=0` on every `_on` row) and reading.*

## What is known

`native::jni::in_native_band_at_pause` asks `conservative_roots::in_native_band_chain_top` for the
outermost JIT entry SP when the narrowing is on, and keeps the whole band `[sp, stack top)` unless that
answer narrows it and the narrowed band still holds the dispatch's spill. On j1 the probe ran on every
banded call (2605) and narrowed none, so one of these answered "keep" every time:

1. the chain was empty, not above `lo`, or its entry rounds up to the stack top (`chain_band_hi`);
2. the read of `[entry_sp, stack top)` would be truncated (`own_stack_band_truncated`);
3. a JIT return address above the chain survived the residue filter (`native_stack_has_jit_frame`
   above `max(residue_hi, entry_sp)`);
4. the narrowed band failed `in_native_band_from` (the spill outside it).

By reading, 2 cannot hold for a band on the thread's own stack (`own_stack_scan_hi` reads the whole
band when `lo >= sp` and `hi <= stack top`), and 4 cannot hold (the spill lies in the dispatch frame,
below every compiled frame and so below `entry_sp`). 1 needs `current_thread_jit_depth() > 0` with an
empty chain, which the band itself rules out. The likely arm is **3**: `ChainHistory::residue_hi`
covers only returned entries' leftovers, and its own doc names the one live guardless JIT frame the
probe exists for (the process entry point, above every entry). A compiled frame that is not in
`JIT_ENTRY_CHAIN` above the outermost registered entry -- an OSR'd `main` loop, or a compiled frame
entered by a path that does not push an entry -- makes every call's probe hit, and then the
narrowing is correctly refused: the band above the chain IS live compiled code. Not verified (no run).

## What was done

`in_native_band_chain_top_why` returns the refusal's reason; the JNI phase census counts each
(`dep_band_narrow_no_chain`, `dep_band_narrow_truncated`, `dep_band_narrow_jit_above`,
`dep_band_narrow_spill_outside`), next to the existing `dep_band_narrow_probe` / `dep_band_narrowed`.

## What would close it

Run NB2. If `dep_band_narrow_jit_above` takes the 2605: find the frame (the
`CRATONVM_DBG_UNREG_MEMO_AUDIT` / A5 census lines name the slot) -- either it is the probe's OSR'd
`main` (then the probe shape, not the rule, is what never narrows: write a probe whose compiled caller
is entered from interpreted code, the cold-memo arm j1 finding 11 already asked for), or a guardless
entry the chain should record (a jit-lane fix). Any other counter: a bug in the rule, fix it here. If
the narrowing still cannot engage on a realistic shape, delete the switch.
