# JIT round 14 lane codecache: proposals

Status: OPEN (proposal book; ideas, not work items)
Area: code-buffer retirement, quiescence evidence, crash diagnostics
Found by: round 14 wave 1 lane codecache

Ranked by expected value.

## CC14-1. Run the `gcd-d10v` loop with `CRATONVM_JIT_POISON_FREE=1` (a measurement, first)

**What.** `platform.rs` (~1903) already has a diagnosis mode that `mprotect`s a retired buffer to
`PROT_NONE` instead of unmapping it, so its addresses are never reused. Under it a stale jump faults
on the FREED buffer's own page, and the crash report's "RECENTLY FREED" line names the right body,
instead of landing in (and being reported against) whatever was mapped there next.
**Benefit.** Turns the ambiguous d10-style report (fault inside a reused range) into an exact one;
together with `CRATONVM_DBG_JIT_CODE_FREE=1`'s new `[jit-code-free-name]` line it names the stale
method in one crash. **Cost.** None to build; address space grows by the retired bytes.
**Risk.** None (diagnosis only). **First step.** Add the arm to the orchestrator's 30x loop.

## CC14-2. Count door exits that withdraw a VALID blocked summary

**What.** Round 14 wave 1 made `jit_execution_leave` withdraw the blocked-stack summary when a door
exits inside a blocked window (`CRATONVM_JIT_BLOCKED_SUMMARY_DOOR_EXIT`). In a balanced world that
branch only ever sees an already-invalid summary; seeing a VALID one is the signature of an
unbalanced `begin_blocking_region` / `mark_blocked_region_enter` (383 call sites). Count those in
`ReclamationCounters` and report them with the code-cache lifecycle report (the production reader
the orphan-instrument check needs); `CRATONVM_DBG_JIT_CODE_FREE=1` prints a backtrace for the
first few. **Benefit.** Finds the unbalanced site instead of papering over it. **Cost.** Small.
**Risk.** None. **First step.** `summary.valid` read under the lock the branch already takes.

## CC14-3. A per-thread compiled-activity stamp for blocked evidence

**What.** Blocked evidence trusts a stack scan until `jit_thread_blocked_leave`. A thread that runs
compiled code inside an unbalanced window invalidates nothing until it passes a door. Stamp a
per-thread counter at every Rust helper entry reached from compiled code (the `contain` wrapper in
`vm/src/jit/helper_guard.rs` is the one choke point) and have `classify_thread_quiescence` treat a
blocked thread whose stamp moved since its scan as running. **Benefit.** Closes the unbalanced-window
hole completely, not only at door exits. **Cost.** One relaxed store per helper call (hot: measure on
`fib` and `hashmap`). **Risk.** Low (retaining direction). **First step.** Only if CC14-2 reads non-zero.

## CC14-4. Report every recent free covering the fault, and flag an entry hit

**What.** `recent_code_free_covering` returns the NEWEST free covering the address. For a fault at a
page-aligned address the older free whose BASE equals the address (a call to that body's entry) is
the likelier culprit (d10: `base+0x3000` of the newest, possibly `base` of an older one). Print up to
three covering frees and mark the one with `base == pc` as "entry". **Benefit.** Correct attribution
without re-running. **Cost.** Small, `vm/src/runtime/crash_handler.rs` (async-signal-safe loop over
the existing ring). **Risk.** None. **First step.** A `recent_code_frees_covering(addr, &mut [..; 3])`
twin of the existing function.

## CC14-5. Make "entered with no owner" unrepresentable on the Rust side

**What.** The Rust twin of r13's CC-1: every Rust dispatch path that calls a raw entry takes a
`RetainedCode` / pin and derives the address from it, so `call_compiled_entry_under_owner(None, ..)`
exists only for non-JIT targets (native trampolines, tests). The exact first step is
`r14w1-codecache-unpinned-raw-entry-call-patch-FIXED-20260929.md`. **Benefit.** Restores the invariant the
whole retirement design rests on ("a thread inside a body owns it") for the helper arms.
**Cost.** Medium (about ten call sites in `helpers.rs`). **Risk.** Low. **First step.** The patch page.

## Round 14 wave 1 (lane codecache): CC14-1 and CC14-5 landed

* CC14-1: `CRATONVM_JIT_POISON_FREE=1` already worked on Linux (`platform.rs` Unix `platform_free`);
  the Windows arm now keeps a retired buffer reserved as `PAGE_NOACCESS` (same counter
  `POISONED_JIT_BYTES`, same fall-through to release on a refused protect). macOS/ARM64 still ignores it.
* CC14-5's first step: `r14w1-codecache-unpinned-raw-entry-call-patch-FIXED-20260929.md` is applied.
