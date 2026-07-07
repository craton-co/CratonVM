# Fork6 — multi-thread (ForkJoinPool worker) JIT-root reclamation

> **RETRACTED 2026-07-07**: the `ElytronRemoteOutboundConnectionTestCase` SIGSEGV
> initially reported here as a new real-world repro of this A4 gap was
> **mis-attributed**. Further investigation (enabling `CRATONVM_DBG_JIT_MIC=1` and
> inspecting the exact pre-crash call sequence) found the real cause: a missing
> autoboxing step in `native_option_map_get` (`native-builtins/src/xnio_async.rs`) that
> let a raw unboxed int (e.g. a `60000`ms timeout option) escape as a bogus pointer-
> shaped `ObjectRef` from an `Object`-returning native method. Every existing GC-root-
> visibility mitigation (`CRATONVM_NO_PRECISE_JIT_MAPS=1`, `CRATONVM_JIT_SAFEPOINT_REG_SPILL=all`,
> `CRATONVM_MOVING_YOUNG=1`+`CRATONVM_SHADOW_STACK=1`) was bisection-tested against the
> real repro and NONE changed the crash -- in hindsight the signal that this was never a
> register-visibility problem. Fixed and merged; see
> [`wildfly-elytron-remoting-segfault-post-keyfactory-fix.md`](wildfly-elytron-remoting-segfault-post-keyfactory-fix.md)
> (now in `docs/internal/fixed-suite-bugs/`) for the full corrected writeup, including
> the "why every mitigation failing is itself a diagnostic signal" lesson. This Fork6/A4
> gap itself remains open and unaffected by that fix.

**Status:** 🟡 OPEN. Non-stress `Fork6`/`Fork6Hard` remains non-reproducing on current `dev`. Two infrastructure bugs adjacent to A4 were found and fixed 2026-07-02 (see that section below) — a takeover gate-polarity bug that made the default-on cross-thread STW JIT scan silently inert, and a defense-in-depth helper-window pass — but neither closes A4 itself, whose register-only residual remains gated on the deferred precise-JIT-stack-maps project. The 2026-07-01 aggressive `GC_STRESS` failures were **NOT A4** (zero live JIT frames, zero compiled JIT code at every STW) — root-caused as three unrelated concurrent-old-gen GC races, now FIXED on dev (`57f545be`); a different residual on that same lane is tracked at [`docs/known-issues/gcstress-residual-corruption-faces.md`](gcstress-residual-corruption-faces.md).

> ## Fix 2026-07-02 — the "default-on" takeover was silently inert; + an initiator-side blocked/helper-window scan; stress lane re-scoped as a separate JIT-free bug
>
> Branch `fix/fork6-xt-helper-window-20260702`, worktree `CratonVM-fork6-helper-20260702`,
> based on dev `69086772`. Unique binaries: `cvmp-fork6-helper-20260702.exe` (fix 1 only),
> `cvmp-fork6-hw2-20260702.exe` (both fixes), `cvmp-fork6-base-69086772.exe` (baseline).
>
> **Fix 1 (the substantive one) — the whole cross-thread STW JIT machinery has been
> a NO-OP in every default-env run.** `CompiledMethodCache::put` registers JIT code
> ranges only when `precise_jit_maps_enabled()` (opt-IN) `||
> cratonvm_jit::xt_jit_root_scan_enabled()`. That jit-crate mirror stayed **opt-IN**
> when the vm-side `xt_root_scan::enabled()` flipped to default-ON: with the env var
> unset, NO code range was ever registered, `jit_code_ranges_snapshot()` was always
> empty, and `take_over_pass` classified every suspended peer as "not in JIT" — the
> takeover, the A5 unregistered-frame fallback inside `scan_active_jit_frames`
> (`native_stack_has_jit_frame` matches against the same empty registry), and the
> cross-thread gap detector were ALL inert. Observed live: `CRATONVM_DBG_XT_JIT_ROOT_SCAN`
> printed `0 code ranges … jit_gate=false` on all 177 STW passes of a Fork6Hard run
> while the vm-side gate reported enabled. Any "default-on takeover" validation that
> did not explicitly set `CRATONVM_XT_JIT_ROOT_SCAN=1` (the Tomcat DoHead runs did
> set it) validated a placebo. Fixed by aligning the mirror's polarity (default ON,
> opt-out `0`/`false`/`off`). After the fix, ranges register as methods compile
> (verified live: 0→1→2 ranges during a Fork6 run, `jit_gate=true`).
>
> **Correction to the 2026-06-29 "precisely located gap": `deposit_root_snapshot`
> DOES scan JIT frames on current dev.** The blocking-deposit path gained the same
> `scan_active_jit_frames` fold as the safepoint path (vm_exec.rs ~1308, landed with
> the Tomcat AQS/ConditionObject work), so a worker that parks via the standard
> deposit protocol (join/park/wait/clinit-wait) publishes its JIT band conservatively.
> The 2026-06-29 audit text below is retained for history but its "deposit does NOT"
> claim is stale. Note `Thread.sleep` is NOT a blocked region at all (raw pump loop,
> stays counted) — the barrier waits for a sleeper, so it has no root gap either.
>
> **Fix 2 (defense-in-depth) — initiator-side helper-window pass.** After the STW
> barrier is satisfied, `helper_window_pass` (`vm/src/jit/xt_root_scan.rs`) suspends
> each remaining peer just long enough to copy its register context + used stack
> into a pre-allocated buffer (never allocating while a peer is frozen — it may sit
> mid-`malloc` inside a helper), resumes it, then classifies offline; only stacks
> carrying a JIT return address contribute conservative roots, and any contribution
> forces the cycle non-moving. This is OS-ground-truth redundancy for the
> deposit-time chain bookkeeping (the chain-desync bug family), NOT the primary
> cover: with the deposit fold present it is normally redundant. Runs only when
> `blocked_count() > 0 && any_thread_in_jit()`. Kill switch:
> `CRATONVM_XT_HELPER_WINDOW_SCAN=0`. Counters: `XT_HELPER_WINDOWS_SCANNED` /
> `XT_HELPER_WINDOW_ROOTS`. Unlike the reverted 2026-06-19b deposit-side scan
> (per-deposit, whole-stack, persistent snapshot growth), this is once-per-collection
> and its roots live only for that collection.
>
> **New deterministic repro: `repros/A4-fork6/HwBlocked.java`** — parks a thread
> (LockSupport.parkNanos, the real blocked protocol; NOT sleep) under a JIT-compiled
> frame whose spill slot is the sole holder of a young String, while sibling threads
> hammer `System.gc()` and churn-allocate token-shaped objects to force freed-block
> reuse. Requires `CRATONVM_JIT_THRESHOLD=100 CRATONVM_BG_COMPILE=0` (the default
> BG-compile pipeline publishes nothing for this shape — see [[project_wire_tiered_manager]];
> also avoid string-concat/`invokedynamic` and train both branches or the method
> deopts). With both fixes: helper-window fires every parked GC (60 windows/run,
> ~30 KB band, ~55 roots) and the run is ALL-OK. With the kill switch off it STILL
> passes — because the deposit fold already roots the token — so this repro
> validates ENGAGEMENT, not failure; A4 remains Java-unvalidatable, as 2026-06-29
> concluded.
>
> **The GC_STRESS lane failures are NOT this bug.** The
> `CRATONVM_DBG_GC_STRESS=65536` Fork6Hard rep-0 failure reproduces while
> `any_thread_in_jit=false` at EVERY STW of the run (`GLOBAL_JIT_DEPTH == 0`: no
> live `JitEntryGuard` anywhere — so no live JIT frame existed, blocked-under-JIT
> included) and with zero compiled JIT code. The stale all-zero receivers /
> `class_id=0` OOB reads under stress come from an interpreter/GC-side mechanism
> (young collections every 64 KiB with real-FJP workers parked in `join()`; prime
> suspect: the blocked-thread pointer-map fold/wake-remap chain under
> multi-GC-while-blocked composition). Lane behavior is unchanged by these fixes:
> rep-0 `ExecutionException: NullPointerException` + `class_id=0` reads, or a
> CPU-pegged wedge (observed ≥7 min on the 2-rep lane, ~40 min on a 5-rep debug
> run), on fix-1-only and both-fixes binaries alike. This supersedes the 2026-07-01
> framing ("this or an adjacent JIT-root coverage failure") the same way 2026-06-29
> re-scoped holder-null: aggressive GC_STRESS keeps finding *adjacent* JIT-free
> bugs.
>
> **UPDATE 2026-07-03 — root-caused and mostly fixed, in a separate branch.**
> The prime suspect above (blocked-thread fold/wake-remap chain) was wrong; the
> real mechanism was the *concurrent old-gen* mark/sweep (`maybe_concurrent_gc`),
> which the earlier investigation never looked at: its SATB write barrier was
> never wired to any production caller (concurrent marking ran with no barrier
> at all), young→old references were never traced as marking roots (an old
> object reachable only through a young holder — exactly what selective
> promotion produces — was swept live), and a failed remark STW silently fell
> through to the sweep with a non-final bitmap. All three fixed and unit-tested
> on `fix/oldgen-concurrent-mark-races-20260703` (`57f545be`), now on dev. A
> *different* residual corruption survives that fix on the same aggressive
> lane — full history in `docs/internal/gcstress-concurrent-oldgen-races-FIXED.md`,
> residual tracked at
> [`docs/known-issues/gcstress-residual-corruption-faces.md`](gcstress-residual-corruption-faces.md).
>
> **Validation (both-fixes binary `cvmp-fork6-hw2-20260702.exe`, real-FJP gate):**
> plain `Fork6` ALL-OK; `Fork6Hard 256 40` ALL-OK; 8-way concurrent `Fork6` 8/8
> ALL-OK; `HwBlocked` ALL-OK with helper-window engaging; bt16 checksum golden
> (14985902). bt16 wall-time vs same-toolchain dev-`69086772` baseline: measured
> under heavy box contention; v2 ≈ v2-gate-off (xt paths add no measurable
> single-thread cost — bintrees never enters the multi-thread STW path).

> ## Re-audit 2026-06-29 (fresh release build off dev HEAD `9928052c`, binary `cvmpjfinish.exe`, JDK-25 oracle)
>
> **A4 task-reclamation is NON-REPRODUCING on current dev.** Across `Fork6` (26/26
> ALL-OK: 8 sequential + 18 concurrent-stress) **and** the aggressive `Fork6Hard`
> (deep N=256–512, both children forked, 200–400 reps, `System.gc()` per rep),
> there were **zero** task-reclamation signatures (`cannot be cast to ForkJoinTask`
> / `trySetException on null` / `nullchild`). The cross-thread STW JIT-root **gap is
> still exercised** (`scan_active_jit_frames` WARN, `cross_thread_jit_gap_hits`
> incrementing) but **non-fatal** — covered by the peer's published `root_snapshot`.
>
> **Why it doesn't reproduce from Java (key finding).** `StrTask.compute`
> (a `RecursiveTask` subclass) is **blocklisted → interpreted** (`is_fjp_subclass_blocklisted`),
> so the forked subtask is an **interpreter local** that `scan_local_objects` /
> `scan_locals_conservative` always root. A4 requires a task held **only** in a
> JIT-compiled **FJP-internal** frame (`ForkJoinPool.runWorker`/`ForkJoinTask.doExec`/
> `WorkQueue.*`) at a safepoint — a deep internal condition **not controllable from
> Java** (hence the historical load-dependence). So A4 cannot be cleanly forced from
> a Java repro, and any fix is **unvalidatable from the Java side**.
>
> **GC_STRESS surfaces a DIFFERENT, separate bug — not A4.** Running `Fork6`/`Fork6Hard`
> under `CRATONVM_DBG_GC_STRESS=65536` fails **deterministically at rep 0** with
> `ExceptionInInitializerError` → `NPE "Cannot read field group because this.holder
> is null"` at `ForkJoinWorkerThread$InnocuousForkJoinWorkerThread.<clinit>` →
> `Thread.getThreadGroup` — i.e. during FJP **worker creation** (`createWorker` →
> `newThread`), **0 task-reclamation signatures**. This is the
> **concurrent-spawn `Thread.holder`-null bug** ([gc-concurrent-spawn-reclamation](../internal/repros/gc-concurrent-spawn-reclamation/),
> BUG-03's `Spawn.java`), which **masks** A4 under stress. It is a separate tracked
> bug, not A4.
>
> **The concrete remaining A4 gap, precisely located.** `update_root_snapshot`
> (running-safepoint path, `interpreter.rs:1835`) publishes a thread's precise JIT
> roots via `scan_active_jit_frames`, but **`deposit_root_snapshot` (the BLOCKED
> path, `vm_exec.rs:1192`) does NOT** — it scans only interpreter `thread.frames` +
> conservative-locals + native pins. So a worker that **blocks in `join()` while its
> compiled `runWorker`/`doExec` frame is the sole holder** of a forked subtask never
> publishes that root. (Currently masked: other roots — deque heap entry, interp
> local — keep the task alive.)
>
> **Why there is no safe+effective quick deposit fix** (and why the earlier full-band
> deposit scan was reverted). The precise marking path's reliable coverage comes from
> the **conservative band backstop** `scan_one_frame([scanner_sp, frame_base))`, not
> the oop-map reads: `PreciseFrameInfo.frame_base` is the **approximate** Rust-guard
> SP (within a few bytes of `entry_sp`), and `exact_rbp` (the true RBP) is reserved
> for the *relocation* walk. So a **precise-only** deposit publish would be
> **unreliable** (approximate base → wrong slots), while the **conservative** version
> **over-retains the whole blocked stack** — the exact regression that reverted
> attempt #1 (2026-06-19b: "12 ok / 12 bad / 6 timeout"). The complete fix therefore
> requires **precise per-PC RBP-chain marking** (each frame's exact RBP + the active
> safepoint's map) so a blocked/peer worker's JIT-frame oops can be marked exactly
> without over-retention — the deferred precise-JIT-maps Stage B/C work, which also
> wants cooperative JIT safepoints (see [bug-03](README.md)).
>
> **Repro added:** `repros/A4-fork6/Fork6Hard.java` (deep, both-children-forked,
> parameterised `N reps`). Under `GC_STRESS` it deterministically reproduces the
> concurrent-spawn `holder`-null bug (BUG-03); with `System.gc()` only it stays
> ALL-OK (A4 non-reproducing). Net: **A4 remains OPEN/architectural** (the
> cross-thread STW JIT-root gap is real) but is **not a reproducible fault on current
> dev**; closing it is gated on the precise per-PC register/RBP-chain marking project,
> not a localized patch.

> ## Fix candidate 2026-07-01 — bounded cross-thread JIT takeover wait
>
> Current `dev` contains a default-on Windows cross-thread JIT takeover path
> (`vm/src/jit/xt_root_scan.rs`): the STW initiator suspends peers whose `Rip` is
> inside registered JIT code, scans their integer registers and stack
> conservatively, excludes them from the barrier quota, and resumes them after
> GC. That is the broad infrastructure direction for this A4 family because it
> captures register-resident peer roots at GC time instead of relying on stale
> snapshots.
>
> The takeover driver had a residual barrier race: it performed several takeover
> passes before `wait_for_all()`, then switched to one unbounded wait. A mutator
> that entered JIT after the final pre-wait pass could still never arrive
> cooperatively, recreating the original STW hang/root-gap window. The fix
> candidate changes the driver to interleave short bounded waits with additional
> takeover passes until the barrier is actually satisfied. New focused coverage:
> `barrier_late_reduce_expected_can_satisfy_bounded_wait`; refreshed coverage:
> `cross_thread_jit_gap_detector_obeys_xt_takeover_gate`.
>
> Status remains fix-candidate until the real `CRATONVM_REAL_FORKJOINPOOL=1`
> Fork6/Fork6Hard lane and the Tomcat real-net/real-AQS classes are soaked on a
> fresh unique binary.

> ## Retry 2026-07-01 -- TLAB-tail hardening plus stress repro
>
> Branch `codex/fork6-fjp-retry-20260701`, based on current local `dev`
> `f7506e02`. Unique binaries used during this retry:
> `target/release/cvmp-fork6-retry-f7506e02.exe`,
> `target/debug/cvmp-fork6-tlabtail-20260701.exe`, and
> `target/debug/cvmp-fork6-stackroots-20260701.exe`. Final post-build sanity
> binary: `target/debug/cvmp-fork6-final-20260701.exe`.
>
> Controls still pass. HotSpot passes `Fork6` and `Fork6Hard 256 20`; CratonVM
> passes plain `Fork6`, `Fork6Hard 256 40`, and a 12-process concurrent
> non-stress lane. Under real FJP, `GC_STRESS=1048576` and `524288` pass on
> `Fork6Hard 128 20`.
>
> Lower stress intervals still fail. `GC_STRESS=262144` and `65536` reproduce
> stale all-zero `Fork6Hard$StrTask` / `ForkJoinTask` receivers, class-id-0
> object reads, `HIB-CV-32` corrupt `CompactValue` guards, non-moving sweep
> re-sync/stopping-walk diagnostics, and occasional SIGSEGV. The older
> "GC_STRESS only exposes the separate holder-null bug" statement is therefore
> superseded for current `dev`: holder-null is gone, but aggressive stress still
> exposes this or an adjacent JIT-root coverage failure.
>
> Changes landed from this retry are defensive, not a closure: terminate spawned
> Java threads with `tlab.retire()` before clearing the published TLAB address;
> retire the TLAB before entering the class-initialization blocked wait; publish
> every live thread's remaining reserved TLAB tail after the STW barrier, not
> only OS-suspended JIT peers; and extend the existing real-FJP/non-moving
> conservative lost-tag scan from interpreter locals to operand-stack slots.
> Focused unit coverage was added for live/unretired TLAB-tail publication and
> lost-tag operand-stack rooting. These changes harden real gaps but do not
> make the aggressive stress lane pass.
>
> Best current suspicion after this retry: a peer can be inside a Rust/runtime
> helper called from JIT, with JIT return frames and task refs on its native
> stack, while its current `Rip` is outside a registered JIT code range. The
> cross-thread takeover path only holds peers whose current `Rip` is in JIT; if
> this helper window reaches GC, roots can be missed without the thread being
> safely frozen. Confirming that needs targeted `xt_root_scan` instrumentation
> or a deterministic worker-state repro before a safe fix.

**Prior status (audit 2026-06-19, retained for history):** 🟡 PARTIAL — the dominant **lost-tag** manifestation is mitigated (conservative interp-local roots under the non-moving sweep, `00429413`) but **only behind the experimental `CRATONVM_REAL_FORKJOINPOOL=1` gate** (the default path is byte-identical baseline). Residual: the worker-forked-subtask reclamation (~15%), then additionally masked by a separate real-FJP `ForkJoinPool` CAS bug. The family-wide precise-JIT-maps default-on (`32649b56`) does **not** close this.

> **Consolidated doc.** This merges the two previous files that described the
> *same* bug from different sessions:
> `precise-jit-stack-maps-multithread-fjp-worker-testcase.md` (the HIB-CV-20
> handoff that provided the repro + toggle matrix) and
> `precise-jit-stack-maps-fork6-findings.md` (the `fix/multithread-jit-roots-stw`
> investigation: root cause, partial fix, residual). Both are now folded in here.
>
> **This is the multi-thread member of the [GC-root-coverage-under-JIT family](README.md).**
> Single-thread sibling = [SB-SUITE-CRASH-04 register-invisibility](SB-SUITE-CRASH-04-jit-inline-new-heap-corruption.md);
> they share the same eventual fix (precise JIT stack roots). Companion project
> note: `project_precise_jit_stack_maps`.

## TL;DR

Under `CRATONVM_REAL_FORKJOINPOOL=1` + JIT, live `ForkJoinTask`s — the root task
that `main` holds as `f`, and worker-forked subtasks — are **reclaimed by the
non-moving young sweep at a `System.gc()`**. The deque/task references decay to
zeroed memory, surfacing as `NullPointerException: Cannot invoke trySetException
on null`, `ClassCastException: java/lang/Object cannot be cast to
ForkJoinTask`, or `gen_heap::get_field: out-of-bounds field read … class_id=0
num_slots=0` (the swept-slot signature).

The earlier "deposit_root_snapshot misses JIT frames" hypothesis (the original
handoff + reverted attempt #1) is **NOT** the cause. The real cause is a
**thread-stack root-coverage gap**, amplified ~3× by **selective-promotion
evacuation**:

- A live object whose only reference is a thread-stack root that the marker's
  *tag-filtered* scan does not capture is (a) not marked and/or (b) not added to
  the selective-promotion **pin set** (pinning is keyed by root value).
- Selective promotion then **evacuates** the un-pinned object to old gen and
  **zeroes the young slot** (`gen_heap.rs::sweep_young_non_moving`, the
  `is_forwarded()` branch). The stale stack reference now reads an all-zero
  header → CCE / SIGSEGV / `[sweep-zero] RECLAIMED-LIVE` on another thread.

## How to reach it (normally invisible)

This path only runs under the opt-in gate `CRATONVM_REAL_FORKJOINPOOL=1` (a
HIB-CV-20 fix on `dev`, commit `d9528134`). With the gate OFF, the synthetic
ForkJoinPool runs every task inline on the calling thread — no real workers, no
real `runWorker`, so the worker-thread JIT roots are never exercised. That is why
no default-config run (bintrees, the app gauntlet) trips this.

The HIB-CV-20 gate (`native-api/src/registry.rs`, `d9528134`) ALSO drops the
synthetic FJP task-family natives (`ForkJoinTask`/`RecursiveTask`/
`RecursiveAction`/`CountedCompleter`) so the real task bytecode runs end-to-end —
which is what makes the **real worker threads actually execute** and exposes this
reclamation. (NOJIT census mode: fully green.)

## Repro

`scratch/xworker/Fork6.java` (`System.gc()`-forced, deterministic ~rep 4) and
`Fork3.java` (racy). A `RecursiveTask<String>` divide-and-conquer combining via
`l + r` (heavy allocation → frequent young GC):

```
CRATONVM_REAL_FORKJOINPOOL=1 CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 \
  target/release/cratonvm.exe --java-home <jdk25> -c scratch/xworker Fork6
```

Symptom in worker threads: `ForkJoinWorkerThread.run(187) ->
ForkJoinPool.runWorker(1992)` throws the NPE / CCE / OOB-field above.
Concurrent stress (`scratch/xworker/stress.sh`) forces the race reliably;
idle single runs almost always pass. `verify-conserv.sh` A/Bs the partial fix.

Self-contained appendix repro (no scratch deps) at the bottom of this doc.

## Toggle matrix (dev + HIB-CV-20 fix, JIT on, gate on)

| toggle | result | what it tells you |
|---|---|---|
| `--Xmx 8g` (suppress young GC) | **PASSES** | it's GC **reclamation**, not codegen |
| `--nojit` | **PASSES** | interpreter frames are precisely scanned; the moving Cheney collector remaps all roots — JIT roots are the gap |
| `CRATONVM_DISABLE_SCALAR_REPLACEMENT=1` | fails (CCE) | **NOT** scalar replacement / the kafka bug-25 escape gap |
| `CRATONVM_NO_SELECTIVE_PROMOTE=1` | bug rate **9/30 → 3/30** | evacuation is the dominant amplifier; residual 3/30 is the pure root gap |
| `CRATONVM_SHADOW_STACK=1` | **fails** — NPE "trySetException on null" + SIGSEGV(139) | current precise roots do NOT cover worker frames |
| `CRATONVM_SHADOW_STACK=1 CRATONVM_SHADOW_PIN=1` | **fails** — CCE + AIOOBE in workers, hang | pin vs movable only changes the corruption shape |

So: definitely the non-moving-sweep-reclaims-a-live-JIT-root problem, but the live
root lives in a **worker thread's** frame and the multi-thread shadow path
doesn't capture/remap it.

## Two confirmed manifestations

1. **`main`'s `f` (lost-tag local).** `main.main` is interpreted (its 200-iter
   loop is below `OSR_THRESHOLD = 1000`), so `f = POOL.submit(t)` is an
   interpreter local. But a JIT-compiled FJP callee returns the task to that
   local under a **non-object tag**, so `Frame::scan_local_objects` (tag-filtered:
   skips `LKIND_LONG/_DOUBLE`, roots only `is_object()` slots) **omits it**.
   Detector signal: `[sweep-zero] … invoked as ForkJoinTask.get/awaitDone`,
   holder `tid=0` (main, the GC initiator). `--nojit` (same `collect_roots`, same
   interpreter) finds `f` — so the lost tag is JIT-path-specific.
2. **Worker forked subtasks.** A running FJP worker (`in_blocked=false`,
   `kind=Platform`) holds `left` in interpreted `Fork6$StrTask.compute` /
   `ForkJoinTask.doExec` / `ForkJoinPool.runWorker` frames; reclaimed at main's
   `System.gc`. Detector signal: `invoked as Fork6$StrTask.fork` / a `checkcast`
   CCE at a deque pop ("java/lang/Object cannot be cast to ForkJoinTask").

## Ruled out (with the experiment that ruled it)

- **deposit_root_snapshot JIT-frame gap** (handoff + reverted attempt #1):
  adding `scan_active_jit_frames` to deposit did not fix Fork6. The reclaim is at
  `System.gc` where the holder is the **initiator** (main) or a **running**
  worker — not a thread blocked in `deposit`.
- **old→young clean-card / barrier miss**: `CRATONVM_DBG_SEED_ALL_OLD=1` (seed
  marking from every old-gen object's young refs) did **not** fix it (still
  9/30). So no live heap object references the reclaimed task — it is a pure
  thread-stack root (consistent with `CRATONVM_DBG_SWEEP_EDGES` being silent).
- **barrier blocked/running exclusion race**: the accounting is sound —
  `BlockedGuard::drop` / `mark_blocked_region_leave` wait out an active STW
  *before* decrementing `threads_blocked`; `request_stw` reads the count and sets
  `stw_requested` under the same `inner` lock as `enter_blocked`.
- **JIT-compiled `compute`**: `is_fjp_subclass_blocklisted` walks the superclass
  chain, so `StrTask` (← RecursiveTask ← ForkJoinTask) is blocklisted ⇒ `compute`
  is interpreted. Not register-invisibility in a JIT'd `compute`.
- **scalar-replacement / selective-promote variant** — both fail (see matrix).

`ForkJoinPool.runWorker`/`WorkQueue` are NOT blocklisted, so `runWorker` IS
JIT-compiled and is the frame holding the popped task. Adding `ForkJoinPool*` to
the blocklist only changed the crash shape (the task-holding frame is also
elsewhere — the external submitter `main`, JIT-compiled, holding the root task
across `invoke`). A blocklist can't fully cover it; precise roots is the real fix.

## Partial fix (branch `fix/multithread-jit-roots-stw`)

`Frame::scan_locals_conservative` + `roots::conservative_locals_enabled`: when the
non-moving sweep will run AND `CRATONVM_REAL_FORKJOINPOOL` is set, additionally
probe every interpreter-frame local's pointer-shaped candidates (object-ptr
decode, `long` payload, raw bits) with the **strict** `is_object_address` header
probe and root them. This catches the **lost-tag** references and — because the
pin set is keyed by root value — **pins** them against evacuation. Sound: only
under the non-moving sweep (nothing relocates), so a false positive can only
over-retain (never corrupt a primitive). Wired into `collect_roots`,
`update_root_snapshot`, and `deposit_root_snapshot`.

**Blast radius:** gated on `CRATONVM_REAL_FORKJOINPOOL` (the gate the bug lives
under), so the default app gauntlet and bintrees are byte-identical to baseline.
Opt out under the gate with `CRATONVM_NO_CONSERVATIVE_LOCALS`.

**Result (40 concurrent-stress runs each):**
- FIX-OFF: 12 bug / 40, `sweep-zero`=46.
- FIX-ON:   6 bug / 40, `sweep-zero`=4.
- bt16 = 14985902 (8.7s), bt18 = 68332206 — no bintrees regression.

So the fix eliminates the dominant **lost-tag** manifestation (`sweep-zero`
46→4) and halves the overall failure rate, with zero default-path impact.

## Residual (~15%, open)

The remaining failures are the **worker forked-subtask** manifestation: a running
worker's interpreted `compute` frame holds the subtask (object-tagged), yet it is
not in the root set at the reclaiming `System.gc` and gets evacuated+zeroed. The
worker is `in_blocked=false` and the barrier accounting is sound, so it *should*
arrive at the STW and self-scan via `update_root_snapshot` — but the subtask is
still missed. Not yet pinned to a mechanism (snapshot freshness/consumption
timing, or a worker arriving via `check_post_block_gc` with a stale deposit
snapshot).

The robust fix the original handoff anticipated — the STW initiator
**conservatively scanning every parked thread's full native stack** (not just its
published tag-filtered snapshot) — is the likely complete solution; it requires
per-thread stack-pointer capture at deposit/safepoint and is a larger change.
This converges with the single-thread CRASH-04 fix: both want precise (or full
conservative) JIT-frame + register roots for the non-moving/selective-promotion
young sweep.

## Update 2026-06-19 — the anticipated "full native stack scan" is REFUTED; the missed root is **register-only**

Measured baseline (gate on, JIT on, conservative-locals fix present, dev
`43033660`): **3 / 24 fail (~12.5%)** — exactly manifestation 2. The crash
signature is `Stale pointer detected in invokevirtual receiver (… all-zero
header)` for a swept `Fork6$StrTask` / `ForkJoinTask`, plus a flood of
reclaimed **worker `Thread` mirrors** (`NoSuchMethodError java/lang/Object.
threadState()/isTerminated()/getThreadGroup()` on `class_id=0` receivers — the
FJP pool-management code calling into reclaimed worker mirrors).

**The handoff's anticipated complete fix — a full conservative native-stack scan
per parked thread — does NOT close it.** `CRATONVM_DBG_FULLSTACK_SCAN=1` (which
makes every `update_root_snapshot` / safepoint self-scan walk the thread's entire
`[scanner_sp, GetCurrentThreadStackLimits.high]` range, validated by
`is_object_address` and pinned by default) was measured at **5 / 20 fail + 1
timeout — no better than baseline.** Since that scan covers *every* qword on the
worker's live native stack (interpreter Rust transients, JIT spill slots, dead
spills — everything between SP and the stack base), the missed `StrTask` oop is
**not on the stack at all.** This matches the doc's own `NO_SELECTIVE_PROMOTE`
result (9/30 → 3/30): the residual 3/30 is a **marking gap** (the object is never
*marked*, so disabling evacuation can't save it), not a pinning/evacuation gap.

**Conclusion: the residual is the *identical* register-invisibility root cause as
the single-thread A3 / SB-CRASH-04** — a live task oop sits **only in a
caller-saved CPU register** of a worker's JIT-compiled `ForkJoinPool.runWorker` /
`ForkJoinTask.doExec` frame at the safepoint, not spilled to the stack and not in
that method's precise oop-map at the call PC. The self-scan path *does* run precise
oop maps (`scan_one_frame_precise`, default-on `CRATONVM_PRECISE_JIT_MAPS`) and the
conservative range scan, but a register-only oop is invisible to both — so it is
never a mark root and the non-moving sweep zeroes its young slot. This is why
`--nojit` passes (no register oops; the moving collector precisely scans + remaps
interpreter frames) and `-Xmx8g` passes (no young GC).

> **Correction (2026-06-29):** the older framing here — that A4 is "the same
> unsolved register-resident problem as A2" — is now only half right. **A2 was
> re-diagnosed and FIXED (`6e3ddb05`, 2026-06-23): it was *not* a register-resident
> root at all, but a GC-side non-moving-sweep free-list double-serve** (overlapping
> free blocks not coalesced). So A2 and A4 were *different* bugs that merely shared
> a symptom (sweep-walk corruption under GC stress). A4's residual genuinely *is* a
> register-only oop at a non-call safepoint (the levers `reg-spill`/`fullstack`/
> `shadow-pin` all fail for it), which is what the original A2 hypothesis predicted
> — that hypothesis was wrong for A2 but is the live theory for A4. On current dev
> `Fork6` is non-fatal (26/26 ALL-OK, re-verified 2026-06-29); the cross-thread STW
> JIT-root gap is exercised (`scan_active_jit_frames` WARN) but covered by the
> peer's `root_snapshot`. A4 is the **last open Family-A member**.

**There is no sound conservative quick-fix** — registers are not on the stack to
scan, and the moving collector (which would remap them) cannot run while any
thread is in JIT without precise per-safepoint oop maps to rewrite the JIT-held
pointers. The complete fix is the **deferred precise-JIT-stack-maps / safepoint
register-spill project** (`project_precise_jit_stack_maps`): generate complete
oop maps for the FJP worker methods (`runWorker`/`doExec`/`WorkQueue.*`) so every
live-oop register is reported (or spilled) at each safepoint. Separately, the
real-FJP `ForkJoinPool` CTL CAS diagnostic (`T19_H6_CAS_DIAG cas_long FAIL
slot=14`) fires during the run but is benign lock-free retry noise (the test
failures are GC reclamation, not the CAS), though it remains a candidate masker
to rule out once the reclamation is fixed.

### Follow-up 2026-06-19b — two more levers REFUTED; the measurement is unreliable

Pursuing the fix further, two more candidate fixes were built/measured and both
**fail**, narrowing the locus but confirming this is the unsolved frontier:

1. **Blocked-path JIT + full-stack scan in `deposit_root_snapshot`.** Hypothesis:
   `update_root_snapshot` (running-safepoint path) calls `scan_active_jit_frames`
   but `deposit_root_snapshot` (blocking path) scans only `thread.frames`, so a
   worker *blocked* in `join()` with a task oop in its JIT `runWorker`/`doExec`
   frame publishes none of it. Added `scan_active_jit_frames` + a new
   `scan_full_native_stack` to deposit, gated on the FJP gate. Result: **12 ok /
   12 bad / 6 timeout / 30 — much WORSE than baseline.** Over-retention (pinning
   the whole blocked stack at every deposit) inflates the young gen → more GCs →
   more reclamation windows for the *still-missed* root, plus hangs. Reverted.

2. **`CRATONVM_JIT_SAFEPOINT_REG_SPILL` (callee-saved blind spill) default-on under
   the gate.** A first 20-run sample read 18/0/2 (looked like a fix) but was
   **load-variance luck** (0 failures in 20 at a ~12.5% rate happens ~7% of the
   time). A controlled 30-run rerun: **18 ok / 7 bad / 5 timeout — no better, with
   added timeouts from the spill overhead.** Spilling *every* used callee-saved GPR
   to a frame slot at each safepoint does **not** capture the missed oop ⇒ the oop
   is **not in a callee-saved register**. It must be caller-saved / `RAX` / an
   operand-stack *scratch* reg, or live at a **non-call (loop back-edge) safepoint**
   where caller-saved regs aren't spilled. (Note the calling-convention tension:
   at a CALL safepoint, caller-saved regs holding a value needed afterwards are
   already spilled by correct codegen — so the surviving suspect is a non-call
   safepoint, or a transient the oop-tracker never tags.)

**Critical blocker for ANY fix: the repro is load-dependent and NOT reliably
deterministic.** The same config measured 0/20 and 7/30 depending on concurrent
system load (other builds/sessions). At a ~12.5% base rate, distinguishing a fix
from noise needs ≳100 interleaved A/B runs per condition, and the failure rate
itself drifts with load — so a partial fix cannot be validated with `Fork6` as-is.
**A more deterministic repro (single forced GC at a pinned worker state, or a
unit-test harness that parks a worker mid-`runWorker` and asserts the task oop is a
root) is a prerequisite** before the precise reg-oop encoding can be implemented
and verified. Net for the next attempt: (a) build a deterministic repro first;
(b) instrument to capture the exact missing oop's register + safepoint PC + method
(extend `CRATONVM_DBG_SWEEP_ZERO` to name the holder register, or single-step the
reclaiming GC); (c) only then add a precise reg-oop map covering caller-saved/RAX
at *non-call* safepoints. Conservative spill-everything levers are exhausted.

## Where to look (multi-thread shadow path)

- `vm/src/runtime/interpreter.rs:1463` — §4 "multi-thread shadow scan, marking
  half": `update_root_snapshot` publishes THIS thread's shadow roots into its
  `root_snapshot` (read by the STW via `collect_all_root_snapshots`).
- `vm/src/runtime/interpreter.rs:1638` — §4 "remap half" in
  `apply_pointer_map_to_thread` (non-initiator threads).
- Open question: when a `commonPool-worker-N` thread is parked at the STW inside
  JIT-compiled `ForkJoinPool.runWorker`, is its shadow stack (a) non-empty with
  the live task oop, and (b) actually scanned as a root by the initiator? Probe
  with `CRATONVM_DBG_SHADOW_DEPTH=1` (per-thread depth at each GC); a worker depth
  of 0 at the GC = the JIT prologue never pushed the task (runWorker/its callees
  aren't shadow-instrumented), so the task is invisible regardless of the
  marking/remap halves.

## Diagnostic tooling added (all gated, default-inert)

- `CRATONVM_DBG_SWEEP_ZERO` now also reports the **GC that reclaimed** the object:
  reason (System.gc / alloc-young / forced-alloc), initiator tid, blocked count
  (`gen_heap` `set_gc_context` + `current_sweep_cycle`), plus the **holder**
  thread id / `in_blocked` / call stack.
- `CRATONVM_DBG_MTROOTS`: per-GC initiator frame-local dump + per-thread
  blocked-state census (`ThreadRegistry::dump_blocked_states`) + a per-thread
  **SELFCHECK** at safepoint-resume naming any thread holding an all-zero
  (reclaimed) reference and its method/blocked-state.

## Suggested acceptance for a fix

`CRATONVM_REAL_FORKJOINPOOL=1` + JIT on + `Fork3`/`Fork6` → ALL-OK over many
concurrent-stress runs (no worker exceptions, no SIGSEGV) — alongside the
single-thread bintrees / `MinRegexProbe` checks. (`-Xmx8g` already passes, so the
bar is "correct at the default small heap where young GC actually runs.")

## Appendix: self-contained repro

`javac Fork6.java`, then run as above with the gate + JIT. Deterministic failure
~rep 4. On HotSpot and CratonVM `--nojit`/`-Xmx8g` it prints `ALL-OK`.

```java
import java.util.concurrent.*;
public class Fork6 {
    static final ForkJoinPool POOL = ForkJoinPool.commonPool();
    static volatile String[] SINK = new String[1];
    static volatile int ROOT_N = 0;
    static final class StrTask extends RecursiveTask<String> {
        final int lo, hi; StrTask(int lo,int hi){this.lo=lo;this.hi=hi;}
        protected String compute(){
            if(hi-lo<=1) return "["+lo+"]";
            int mid=(lo+hi)>>>1; StrTask left=new StrTask(lo,mid); left.fork();
            String r=new StrTask(mid,hi).compute(); String l=left.join();
            if(l==null||r==null) throw new IllegalStateException("nullchild["+lo+","+hi+")");
            String res=l+r; if(lo==0&&hi==ROOT_N) SINK[0]=res; return res;
        }
    }
    public static void main(String[] a){
        int N=64; ROOT_N=N; StringBuilder sb=new StringBuilder();
        for(int i=0;i<N;i++) sb.append("[").append(i).append("]");
        String want=sb.toString();
        for(int rep=0; rep<200; rep++){
            SINK[0]=null; Object got;
            try { StrTask t=new StrTask(0,N); ForkJoinTask<String> f=POOL.submit(t);
                  System.gc(); got=f.get(); }
            catch(Throwable e){ System.out.println("rep="+rep+" THREW "+e); return; }
            if(!want.equals(got)){ System.out.println("rep="+rep+" FAIL got="+got
                +" sinkOk="+want.equals(SINK[0])); return; }
        }
        System.out.println("ALL-OK");
    }
}
```

A reflective dump of a failed task is the giveaway: `RecursiveTask.result` is the
correct `String` via reflection, but `getRawResult()` / a worker's deque pop
returns a bare `java.lang.Object` (a reclaimed/zeroed slot).
