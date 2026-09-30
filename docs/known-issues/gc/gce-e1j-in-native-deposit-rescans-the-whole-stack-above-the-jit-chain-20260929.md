# Every JNI native call under the in-native package probes the whole stack above the JIT chain, once or twice

> **STATUS (2026-09-29, gce e1/x): KEEP -- the fix is engaged and sound, but the JNI pages are held open until the follow-up measurement.** Unit tests pass (e1 Windows suite). Generational package run: `[GC] unreg_memo: shortcircuits=1425527 SUPPRESSED=0`; `*_jnimemo_audit` = HotSpot on all three collectors (`verify-e1/ve1`). The expected cost drop did not happen (`noop` 5.1-7.3 us vs 5.3-6.8 us on base): the band probe was not where the time goes. **Remaining:** nothing on this page's defect by the evidence; it is held with the JNI group (follow-up e1b halved the Generational package cost but regressed the default arm on G1/ZGC); retire it once the `CRATONVM_DBG_JNI_PHASE=1` census shows no residual whole-stack phase.

> **STATUS (2026-09-29, gce e1/j): FIXED IN CODE, awaiting the probe -- a
> frozen-band memo, active only inside the package's in-native deposit.**
> Filed and fixed by reading (no cargo in the lane). With the defaults
> nothing changes: the memo is armed only by `native_call_enter_native`
> (`vm/src/native/jni.rs`), which runs only with
> `CRATONVM_JNI_NATIVE_TRANSITIONS=1`.
> Verify: `cargo test -j 5 -p cratonvm-vm --lib frozen_band_memo_holds_only_while_the_chain_is_untouched`,
> then `Gcd1JniCostProbe` arms A / P as in the report
> (`docs/internal/gc-design-perf-round-20260929/e1-j-report.md`, section 2).

*Filed 2026-09-29 by gce e1/j, by reading. Severity: MEDIUM (performance
only; it is most of the per-call cost that keeps the JNI in-native package,
and with it the fix of four JNI GC defects, off by default).*

## Evidence

With `CRATONVM_JNI_NATIVE_TRANSITIONS=1`, every JNI native call deposits the
thread's roots on the way into native (`native_call_enter_native` ->
`NativeContextImpl::deposit_root_snapshot`, `vm/src/vm/vm_exec.rs`). The
deposit:

1. calls `crate::jit::conservative_roots::invalidate_scan_cache_for_gc()`,
   which resets `UNREG_JIT_MEMO` to "nothing verified"
   (`vm/src/jit/conservative_roots.rs`, `unreg_memo_gc_reset_enabled`, default
   on). The reset's own doc prices it as "one full band rescan per
   GC-authoritative root collection, not per native call" -- true when only
   parking threads deposited, false once every JNI call does;
2. on Generational, calls `refresh_moving_young_coverage_for_current_thread`,
   whose A5 block runs `native_stack_has_jit_frame(cover_hi, stack_top)`
   UNMEMOISED (the block's own note: "Routing this call site through the
   memo measured inert (reverted)");
3. unless the precise-only path is taken (never on G1 / ZGC), calls
   `scan_active_jit_frames`, whose A5 block probes the same band again (the
   memo was just reset, so `UnregScan::Detect { hi: None }`).

`[cover_hi, stack_top)` is every interpreter and Rust frame of the thread
above its outermost compiled entry. On `DefaultCatalogAndSchemaTest` the probe
read 35.2 billion words in 1.14 million calls (about 30 000 words, 240 KiB, per
call; comment above `thread_chain_proven` in `deposit_root_snapshot_inner`).
`Gcd1JniCostProbe noop` (a compiled loop calling an empty native; stack
`main` -> `measure` -> the lambda's compiled `run`) pays it on every call:
6.5 us per call with the package against 0.8 us without (gcd d10
verification, Generational). This was not on the d10/j proposal's list of
where the deposit's time goes (`gcd-d10j-proposal-in-native-entry-without-a-deposit-20260928.md`
names the band BELOW the chain, the coverage proof's chain walk, the frame
trace and the frame scan).

## Why a memo is exact here

Across the calls a compiled loop makes into a native, the band above
`cover_hi` is frozen. The outermost chain entry's `JitEntryGuard` frame sits
at `cover_hi`; while that entry stays on the chain, the thread runs only below
it, so no `call` writes a return address above it and every frame above it
is a suspended one that was already there. Data written into a suspended
frame through a pointer, or a new code range, can only add a false positive
(the module's own argument at `UnregMemo::observe`), never hide a live frame.
So a clean verdict over `[cover_hi, high)` holds while the chain was not
pushed, popped or pruned and `cover_hi`, the stack top and the code-range
generation are unchanged.

## What landed (gce e1/j)

`vm/src/jit/conservative_roots.rs`:
- `JIT_RESIDUE_HI` became `JIT_CHAIN_HISTORY` (`ChainHistory`: the residue
  mark, unchanged; a chain mutation count bumped by `push_entry_full`,
  `pop_jit_entry` and a pruning `prune_returned_jit_entries`; the armed bit;
  the memo). There is still one thread-local, so the statics ratchet is
  unchanged.
- `with_frozen_band_memo` arms the memo for one deposit.
- `frozen_band_key` / `frozen_band_clean` / `note_frozen_band_clean`: the key
  is (mutations, `cover_hi`, stack top, `jit_code_ranges_generation()`). It
  is refused for an empty chain, for a band that does not start at
  `cover_hi`, and for an SP that is off the thread's own stack.
- The coverage proof's A5 probe is skipped on a hit. It records the
  classified verdict of a whole, untruncated probe (a residue hit its filter
  discards counts as clean, since the residue mark moves only with a pop).
- `scan_active_jit_frames`' A5 block treats a hit as `already_clean`. It
  accepts only a RAW-clean verdict, because with a non-empty chain it marks
  the band on any hit, residue included. It records the raw verdict of a
  whole-band probe.
- The memo's audit: under `CRATONVM_DBG_UNREG_MEMO_AUDIT=1`, a hit is counted
  in `UNREG_MEMO_SHORTCIRCUITS` and re-probed. A live hit lands in
  `UNREG_MEMO_SUPPRESSED`, printed at exit with `CRATONVM_GC_STATS=1` as
  `[GC] unreg_memo: shortcircuits=N SUPPRESSED=0 ...`.

`vm/src/native/jni.rs`: `native_call_enter_native` runs its deposit inside
`with_frozen_band_memo`. That covers the native call's entry and the full
re-entry after a JNIEnv function. The re-entry after an up-call finds the
chain mutated, so the memo misses and the band is probed afresh.

## Residual

A leaked chain entry is one whose guard's drop was bypassed. When it sits
above the thread's real frames, `prune_returned_jit_entries` does not remove
it. With such an entry on the chain, the thread could return above `cover_hi`
and call compiled code without a guard, so without any push. The chain band
scan beside this already trusts the chain in that case.

The audit above is how the residual would show: a non-zero `SUPPRESSED`.

## How to verify

- Unit: `cargo test -j 5 -p cratonvm-vm --lib frozen_band_memo_holds_only_while_the_chain_is_untouched`.
- Cost: `Gcd1JniCostProbe` arms A and P, interleaved, on each collector. The
  commands and the expected numbers are in section 2 of
  `docs/internal/gc-design-perf-round-20260929/e1-j-report.md`.
- Correctness, arm P, each of `-XX:+UseGenerationalGC`, `-XX:+UseG1GC`,
  `-XX:+UseZGC`, 3 runs:
  - `Gcd1JniRootsProbe` and `Gcd1JniBlockInNativeProbe` print HotSpot's
    lines.
  - With `CRATONVM_XT_ROOT_SCAN_AUDIT=1`, `grep -c 'ROSTER HOLE'` = 0.
  - With `CRATONVM_DBG_UNREG_MEMO_AUDIT=1 CRATONVM_GC_STATS=1`, stderr's
    `[GC] unreg_memo:` line reads `SUPPRESSED=0` with `shortcircuits` > 0.
