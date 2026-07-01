# OPEN: Tomcat start/stop is wait-bound (~2.86s/iteration) — DoHead family too slow

**Status:** OPEN (perf). Spawned-task id: `task_0722e093`.
Full investigation + profiling: memory note `reference_tomcat_dohead_gc_safepoint_deadlock`.

## Problem
The Tomcat `TestHttpServlet`/`TestHttpServletDoHead*` family runs ~156 sub-tests,
each a **full Tomcat start + HTTP exchange + stop**, at **~2.86s/sub-test
(~450s/class)** — too slow to finish within the suite's 600s timeout under
`-Parallel 4`. On HotSpot each iteration is far faster.

## Key finding (already profiled — do not redo unless verifying)
The iteration is **WAIT-BOUND, not CPU-bound**. samply+ETW on a steady-state run:
the process uses only **~1.5 cores**; the test thread (main-vm) is **~40% on-CPU /
~60% WAITING**. The CPU side (class-resolution eviction) is already addressed
(`LinkResolver` cache cap 16k→128k, merged `79dfe23c`). The remaining wall-time is
the **60% WAIT**: Tomcat `stop()` thread-joins (acceptor/poller/exec-pool
termination) + HTTP round-trips. So the lever is **faster thread shutdown**, not CPU.

## Where to look
- `vm/src/threading/thread_registry.rs` — `join()` latency.
- `native-io/src/{nio_selector,socket_channel,net}.rs` — acceptor woken from blocking
  `accept()` on stop (listener close / interrupt), poller `selector.wakeup`, socket close.
- Tomcat executor shutdown await-termination timeouts.
Get an off-CPU/wait profile (or instrument) of one start/stop to confirm which phase
dominates (start vs HTTP exchange vs stop).

## Repro / measure (PowerShell, from `apps/tomcat-suite-runner`)
- `handle-monitor.ps1` — prints handles/threads/cases every 10s; steady-state ≈ 2.86s/case.
- `run-tomcat-suite.ps1 -Start 28 -Count 1 -TimeoutSec 700` — run a class to completion.
Env: `CRATONVM_REAL_NET_SOCKETS=1`, `CRATONVM_REAL_AQS=1`, `CRATONVM_ROOTSNAP_CACHE=1`.
Build a uniquely-named binary in a separate worktree.

**Goal:** cut per-sub-test wall-time (target <2s) by reducing the dominant WAIT so the
family completes within 600s under Parallel-4, without regressing other apps.
NB: a separate issue (`tomcat-dohead-blocked-thread-gc-sweep-corruption.md`) also
blocks full green; this one is purely speed.
