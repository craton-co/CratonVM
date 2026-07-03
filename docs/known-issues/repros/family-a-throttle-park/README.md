# MiniThrottle — Family-A residual repro (parked-thread lock churn, no Spring)

Self-contained mirror of Spring's `ConcurrencyThrottleInterceptorTests`
(`spring-aop`): a JDK dynamic proxy whose `InvocationHandler` applies a
`ReentrantLock`+`Condition` throttle (limit 1, then 10), hammered by 100
threads × 1000 proxied `getName()` calls plus 10 exception-throwing threads
through the same chain. HotSpot: `ALL THREADS JOINED` in <1 s.

```
javac MiniThrottle.java
$CV --java-home "$JDK25" -cp <dir> MiniThrottle
```

Status (2026-07-03):

- **Pre-fix dev** (before `fix/throttle-jit-concurrency-hang`): deterministic
  JIT-mode WEDGE — "inconsistent header / array_length=…" corruption storm,
  `gen_heap::get_field: out-of-bounds field read dropped` on a live
  `AQS$ConditionNode`, then all threads parked forever (rc=124). Same
  mechanism as the Spring test: stale `JIT_SCAN_CACHE` roots at the
  blocked-path `deposit_root_snapshot` → un-pinned ConditionNode moved by
  selective promotion under a live JIT spill slot.
- **Post-fix**: the Spring test passes 6/6 runs (both JIT modes) — but this
  leaner, higher-GC-frequency shape STILL fails: across 5 fixed-binary JIT
  runs (varying machine load), 1× SIGSEGV (read at a garbage forwarding
  target `0x20000000000` after "forwarded young object … has non-old-gen
  target" containment) and 4× rc=124 timeouts with corruption warnings
  ranging 0–5583. The residual is the Family-A running-thread /
  register-invisibility root gap tracked by A4 and
  `dohead-jit-heap-corruption-register-invisibility.md` — NOT the fixed
  deposit-cache staleness (whose deterministic parked-ConditionNode
  `get_field num_slots=0` wedge signature is gone).

Diagnostic leads from the fixed-binary A/B matrix (one 420 s-capped run each,
loaded box — treat as leads, not conclusions):

| knob | corruption warns | outcome |
|---|---|---|
| (none) | 165 / 438 / 5583 | timeout ×3, SIGSEGV ×1 |
| `CRATONVM_REAL_FORKJOINPOOL=1` (enables `scan_locals_conservative`) | **0** | timeout (finished pass 1) |
| `CRATONVM_DBG_FULLSTACK_SCAN=1` | 6 | timeout (finished pass 1) |

The conservative-locals gate zeroing the corruption suggests the residual's
dominant member here is the **lost-tag interpreter-frame local** (same as
Fork6 manifestation 1), not a register-only oop — worth pursuing an un-gated
(quiescence-conditional) `scan_locals_conservative` with bt16/bt18
regression gates.

Use this repro (not the Spring classpath) to iterate on the residual: it needs
no suite classpath and fails in under ~2 minutes when it fails.
