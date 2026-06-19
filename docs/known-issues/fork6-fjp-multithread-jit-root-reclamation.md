# Fork6 — multi-thread (ForkJoinPool worker) JIT-root reclamation

**Status:** 🟡 PARTIAL (audit 2026-06-19) — the dominant **lost-tag** manifestation is mitigated (conservative interp-local roots under the non-moving sweep, `00429413`) but **only behind the experimental `CRATONVM_REAL_FORKJOINPOOL=1` gate** (the default path is byte-identical baseline). Residual **OPEN**: the worker-forked-subtask reclamation (~15%) remains, now additionally masked by a separate real-FJP `ForkJoinPool` CAS bug. The family-wide precise-JIT-maps default-on (`32649b56`) does **not** close this — see [README](README.md) A4 = OPEN/inconclusive.

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
interpreter frames) and `-Xmx8g` passes (no young GC). It is the **same unsolved
problem** recorded for A2/A3 in `reflrepro-register-resident-jit-root-handoff.md`,
where *every* coverage lever (`reg-spill`, `fullstack`, `shadow-pin`) also fails.

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
