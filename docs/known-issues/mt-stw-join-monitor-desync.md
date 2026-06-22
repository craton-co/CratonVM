# Multi-thread STW residual: `Thread.join` monitor-ownership desync under concurrent GC

**Status:** OPEN (rare, ~1–3% on the `scratch_churn/Churn.java` repro). Distinct
from — and deeper than — the four multi-thread-STW barrier/expansion bugs fixed
in commit `68c6993e` (merged to `dev` as `f6186169`), which took the same repro
from **0% → ~97%** pass. This doc is the precise root cause of the remaining tail,
captured with a non-invasive `cdb` native-stack catch plus gated VM instrumentation
(`CRATONVM_DBG_MONIMSE`, since reverted).

## Repro

`scratch_churn/Churn.java`: 6 `Thread.start` workers, each
`for(i<4000){ Integer.toString(...); if(i%64==0) System.gc(); }`, then
`main` does `for(t: ts) t.join()`. JIT **off**, IR-FP **off**. Normal completion
≤ 6 s; a hung run never completes (true livelock, not slowness — confirmed at a
90 s ceiling, max successful duration 6 s).

## Symptom

`main` livelocks inside `java.lang.Thread.join(long)` constructing
`IllegalMonitorStateException` in a tight loop (observed **2823–4728 IMSE
allocations** in ~7 s before the watchdog aborts). It never reaches a GC
safepoint, so the next `System.gc` initiator's `wait_for_all` waits for `main`
forever → the whole VM wedges (every worker parks in `arrive_and_wait`, the
initiator parks in `wait_for_all`).

## Mechanism (why the loop is infinite)

JDK `Thread.join(0)` is `synchronized` via **explicit** `monitorenter`/`monitorexit`
bytecode, with the javac synchronized-exit handler:

```
Exception table:  from  to  target type
                    61  137   140   any   ; try body
                   140  144   140   any   ; the handler guards its OWN monitorexit
```

The loop body is `while(isAlive()) wait(0)`. When `Object.wait(0)` (pc 129) throws
`IllegalMonitorStateException` because `main` does not own the monitor, control
goes to the handler at pc 140, which runs `monitorexit` (pc 143) — which **also**
throws IMSE (still not owner) — which routes via the `[140,144)→140` entry **back
to pc 140**. This is correct JDK bytecode + correct JVM exception semantics: the
infinite loop is the inevitable response to a `monitorexit` that *perpetually*
fails. HotSpot does not hang here only because there the ownership is intact.
**So the loop is a symptom; the root cause is the lost ownership.**

## Root cause (why ownership is lost)

`CRATONVM_DBG_MONIMSE` instrumentation at the IMSE throw sites caught two shapes,
both for the Thread object `main` is joining:

1. `wait NOT-OWNER ... mark=<inflated> mon_owner=None` — the object is a **valid
   live heap object** (`heap_obj=true`), all of `main`'s frame locals hold it with
   **correct object tags** (`is_object_value=true`, so NOT a lost-tag/stale-pointer
   missed-root), `main` is still `in_blocked=true`, yet the object's **inflated
   monitor has `owner=None`**.
2. Later in the storm: `exit THIN-fail ... mark=0x0` — the same address now reads
   an all-zero (NEUTRAL) mark word, i.e. freed/reset young-from-space.

`MonitorTable::inflate_locked` produces an `owner=None` monitor **only from a
NEUTRAL mark word** (the THIN branch pre-acquires for the thin owner; only the
NEUTRAL branch creates an unowned monitor). So between `main`'s `monitorenter`
and a later `wait`, the object's monitor got **re-associated to a fresh
`owner=None` monitor** — the object's lock state was lost across a concurrent
moving GC, then re-inflated unowned. This is a **monitor-identity / mark-word
desync across a moving collection**, not a missed root and not a stale frame ref.

All GC object-copy paths *do* preserve the mark word (atomic load+store in
`gen_heap::forward_object`, the promotion path, `g1.rs`, `gc.rs`), and
`MonitorTable::remap_after_gc` re-keys the registry correctly (reclaim is
default-OFF, so live monitors are never dropped). The desync therefore comes from
a **mutator mutating the Thread object's monitor/mark word while GC-blocked,
concurrently with the collector** — the prime suspect is the dying worker's
death-notify (`vm/src/vm/vm_exec.rs` thread_start terminate tail:
`enter_or_contend` + `notify_all` + `exit` on `wake_obj`, performed inside the
`enter_blocked` termination region, where a concurrent STW collector may relocate
and re-key that very object). `wake_obj` is read from the registry just before
`mark_dead`; after `mark_dead` the registry no longer remaps it, so a GC during
the notify can also leave `wake_obj` stale.

## Secondary amplifier

`NativeContextImpl::monitor_wait` (vm_exec.rs) propagates the `wait` `Err` via `?`
**before** calling `check_post_block_gc()`:

```rust
let was_interrupted = { let blk = enter_blocked(); ...; let r = monitors.wait(...); drop(blk); r }?;
// check_post_block_gc() is SKIPPED when monitors.wait() returns Err
self.check_post_block_gc();
```

`drop(blk)` decrements `threads_blocked`, but the skipped `check_post_block_gc`
leaves `gc_block_state.in_blocked_region = true` and the snapshot/fixup unapplied.
The thread is then in an inconsistent state: a *new* STW counts it in `expected`
(it is no longer in `threads_blocked`) yet `fold_pointer_map_into_blocked` still
treats it as blocked — and it is livelocking the IMSE storm, so it never arrives.
This is what turns the IMSE livelock into a full barrier wedge. Fixing this alone
is insufficient (the IMSE storm runs in the exception-dispatch loop, which polls
`stack_dump` at the top of `execute_frame` but **not** the GC safepoint — that
only happens at backward branches — so the thread still never arrives, and even
if it did it would keep storming and never print "Churn done").

## Why the foreign-attach soak is unaffected

`libcratonvm` `foreign_attach_concurrent_gc_soak` exercises **foreign/host**
threads doing concurrent `System.gc`; they detach via the **host** join, never
Java `Thread.join`. The concurrent form was restored (all workers initiate, not
just `t==0`) once the four barrier bugs were fixed — see the comment at the
`if i % 64 == 0` site. This residual does not gate that soak.

## Fix direction (for a future, isolated change)

- Make the dying worker's death-notify safe against a concurrent moving
  collection: perform the monitor `notify_all` such that it cannot race the
  collector's relocate+re-key of `wake_obj` (e.g. as a true STW participant, or
  pin/re-resolve `wake_obj` across the operation), so the Thread object's monitor
  identity/ownership is never desynced.
- Independently, call `check_post_block_gc()` on the `monitor_wait` error path so
  a thread leaving `Object.wait` via IMSE still exits the blocked region cleanly.

Both are needed: the first removes the IMSE; the second removes the barrier
inconsistency it would otherwise amplify.
