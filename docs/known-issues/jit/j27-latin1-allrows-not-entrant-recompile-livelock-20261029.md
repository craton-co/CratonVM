# J27Latin1CharRows `--jdk-only` hangs ~4% of runs on Linux: not-entrant re-entry / stale-caller recompile livelock grows the retire queue without bound

Filed 2026-10-29 by the j27 orchestrator (snapshot 17 battery). **Dev-owned**: the
pure `origin/dev` tree (c386bad829) reproduces it; the j27 round adds nothing to it.
Behaviour gap (a hang, HotSpot finishes in ~1 s). Not fixed.

## Symptom

`tools/probes/jit/j27/J27Latin1CharRows.java`, `--jdk-only`, Linux host 64.177
(release, LTO off, CGU 16): the run prints the first 3 of 30 fact lines
(`latin1|masked|char isDigit`) and then never finishes the `all isDigit` row
(`Character.isDigit(int)` over -16 .. 0x11000F, 3 reps). A normal run takes 1-3 s.
Windows did not reproduce it in 40 runs (cvm-rpl-4.exe).

## Frequency (40 s watchdog, 4 runs in parallel, `l1g.sh`)

| binary | tree | hangs |
|---|---|---|
| cvm-j27-base | round start (2026-10-28 12:28) | 0/40 |
| cvm-j27-dev | older dev (2026-10-28 18:34) | 0/40 |
| cvm-j27-s8, hf1 | j27 waves 1-8 | 0/40 each |
| cvm-j27-s13 | waves 1-8, before the dev merge 47479dc6e0 | 0/120 |
| cvm-j27-s14 | s13 + merge of dev 53c3f9caee | 1/80 |
| cvm-j27-s15 | s14 + wave 9 | 2/40 |
| cvm-j27-s17 | s15 + merge of dev c386bad829 | 3/40, 2/40 |
| cvm-j27-devtip | **exactly the origin/dev c386bad829 tree** | 3/80 |

So it entered with dev's side of the 47479dc6e0 merge (71 dev commits; the JIT
ones are 53c3f9caee vector-reduction iota lanes, 8335e345ff service-copy window
at owned-args re-run points, 9936c1a7a6 RAX dead at helper calls, 6a4024ffa4 drop
JIT holders, fae7dfa7be boot mapping via the image broker). Not bisected further.

## Where it is stuck (gdb `thread apply all bt`, both s17 and devtip)

Thread `main-vm`, sampled twice, always:

```
memcpy / realloc (Vec<DeferredOwner> grow, 2.3 MB)
drain_deferred_jit_owners          jit/src/lib.rs   (partition of the WHOLE queue)
enqueue_retired_batch / enqueue_retired / defer_jit_owner
drop_glue<RetainedCode>
stale_caller_recompile_on_stub_pass   vm/src/runtime/interpreter/jit_bridge.rs
jit_not_entrant_entry_body            vm/src/jit/helpers/thread.rs
jit_not_entrant_entry                 (called from compiled code)
```

Every other thread is idle (compiler workers parked on the empty queue). Reading:
compiled code keeps calling a not-entrant body; each entry recompiles the stale
caller and retires a body; the retiring thread is itself inside compiled code, so
`ACTIVE_JIT_EXECUTIONS` is never zero and per-thread evidence keeps every entry
(`kept_by_running_thread`), so the queue only grows and each drain re-partitions it
(O(n^2)). The root defect is the not-entrant re-entry loop (the caller is
recompiled but the call site keeps reaching the not-entrant stub); the unbounded
queue is what turns it from "slow" into a wall-clock hang.

## Reproduce

```
cd <dir with J27Latin1CharRows.class>
for i in $(seq 1 40); do timeout 40 cratonvm --jdk-only -cp . J27Latin1CharRows | grep -c ^fact; done
# 30 = ok, 3 = hung (rc 124)
```

Next step for the owner: `CRATONVM_DBG=deopt,jit-compiled` on a 40-run loop (may
flip the bistable outcome, see the memo on debug flags), count re-entries per
caller in `stale_caller_recompile_on_stub_pass`, and bisect the five dev JIT
commits above with the same 80-run loop.
