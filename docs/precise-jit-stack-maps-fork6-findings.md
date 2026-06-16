# Fork6 FJP multi-thread reclamation — root cause, partial fix, residual

Investigation on branch `fix/multithread-jit-roots-stw` (worktree `C:/craton/CratonVM-mtroots`,
base dev `8e8e47d9`). Companion to
`docs/precise-jit-stack-maps-multithread-fjp-worker-testcase.md` (the handoff that
provided the Fork6 repro) and `project_precise_jit_stack_maps`.

## TL;DR

Under `CRATONVM_REAL_FORKJOINPOOL=1` + JIT, live `Fork6$StrTask` objects (the
root task `main` holds as `f`, and worker-forked subtasks) are **reclaimed by the
non-moving young sweep at a `System.gc()`**. The earlier "deposit_root_snapshot
misses JIT frames" hypothesis (handoff + reverted attempt #1) is **NOT** the
cause. The real cause is a **thread-stack root-coverage gap**, amplified ~3× by
**selective-promotion evacuation**:

- A live object whose only reference is a thread-stack root that the marker's
  *tag-filtered* scan does not capture is (a) not marked and/or (b) not added to
  the selective-promotion **pin set** (pinning is keyed by root value).
- Selective promotion then **evacuates** the un-pinned object to old gen and
  **zeroes the young slot** (`gen_heap.rs::sweep_young_non_moving`, the
  `is_forwarded()` branch). The stale stack reference now reads an all-zero
  header → CCE / SIGSEGV / `[sweep-zero] RECLAIMED-LIVE` on another thread.

This is why the toggle matrix behaves as it does:
- `--Xmx 8g` passes: no young GC, no evacuation.
- `--nojit` passes: no JIT frames ⇒ `gc_quiescence` inactive ⇒ the **moving**
  Cheney collector runs, which does a full trace and remaps **all** roots
  (frames + heap) via the pointer map — no card/pin dependency.
- JIT-on fails: `gc_quiescence` active ⇒ the **non-moving + selective-promotion**
  sweep runs, which depends on the root/pin set being complete.

## Two confirmed manifestations

1. **`main`'s `f` (lost-tag local).** `main.main` is interpreted (its 200-iter
   loop is below `OSR_THRESHOLD = 1000`), so `f = POOL.submit(t)` is an
   interpreter local. But a JIT-compiled FJP callee returns the task to that
   local under a **non-object tag**, so `Frame::scan_local_objects` (tag-filtered:
   skips `LKIND_LONG/_DOUBLE`, roots only `is_object()` slots) **omits it**.
   Detector signal: `[sweep-zero] ... invoked as ForkJoinTask.get/awaitDone`,
   holder `tid=0` (main, the GC initiator). Note `--nojit` (same `collect_roots`,
   same interpreter) finds `f` — so the lost tag is JIT-path-specific.
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
  *before* decrementing `threads_blocked`, so a thread cannot execute bytecode
  while counted blocked/excluded. `request_stw` reads the count and sets
  `stw_requested` under the same `inner` lock as `enter_blocked`.
- **JIT-compiled `compute`**: `is_fjp_subclass_blocklisted` walks the superclass
  chain, so `StrTask` (← RecursiveTask ← ForkJoinTask) is blocklisted ⇒ `compute`
  is interpreted. Not register-invisibility in a JIT'd `compute`.

## Decisive amplifier experiment

`CRATONVM_NO_SELECTIVE_PROMOTE=1` (disable evacuation; pure non-moving sweep):
Fork6 bug rate **9/30 → 3/30**. With evacuation, an un-pinned but heap-reachable
object is moved and its young slot zeroed → hard failure; without it, the same
object survives in place. The residual 3/30 is the pure root gap (objects not
reachable from the marked set at all).

## Partial fix (this branch)

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
under), so the default app gauntlet and the bintrees benchmarks are
byte-identical to baseline. Opt out under the gate with
`CRATONVM_NO_CONSERVATIVE_LOCALS`.

**Result (40 concurrent-stress runs each):**
- FIX-OFF: 12 bug / 40, `sweep-zero`=46.
- FIX-ON:   6 bug / 40, `sweep-zero`=4.
- bt16 = 14985902 (8.7s), bt18 = 68332206 — no bintrees regression (and inert
  there anyway without the FJP gate).

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
snapshot). The robust fix the original handoff anticipated — the STW initiator
**conservatively scanning every parked thread's full native stack** (not just
its published tag-filtered snapshot) — is the likely complete solution; it
requires per-thread stack-pointer capture at deposit/safepoint and is a larger
change.

## Diagnostic tooling added (all gated, default-inert)

- `CRATONVM_DBG_SWEEP_ZERO` (pre-existing) now also reports the **GC that
  reclaimed** the object: reason (System.gc / alloc-young / forced-alloc),
  initiator tid, blocked-thread count (`gen_heap` `set_gc_context` +
  `current_sweep_cycle`; consumed in the interpreter detector) — plus the
  **holder** thread id / `in_blocked` / call stack.
- `CRATONVM_DBG_MTROOTS`: per-GC initiator frame-local dump + per-thread
  blocked-state census (`ThreadRegistry::dump_blocked_states`) + a per-thread
  **SELFCHECK** at safepoint-resume that names any thread holding an all-zero
  (reclaimed) reference and its method/blocked-state — the tool that pinned the
  holder to running FJP workers.

## How to reproduce / measure

`scratch/xworker/stress.sh` (concurrent oversubscription forces the
timing-sensitive race reliably; idle single runs almost always pass).
`scratch/xworker/verify-conserv.sh` A/Bs the fix vs
`CRATONVM_NO_CONSERVATIVE_LOCALS=1`.
