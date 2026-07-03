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
- **Post-fix**: the Spring test passes 6/6 runs (both JIT modes), and this
  repro usually completes — but being a leaner, higher-GC-frequency shape it
  can STILL trip the residual Family-A member (observed ~1/4: SIGSEGV, read
  at a garbage forwarding target `0x20000000000` after "forwarded young
  object … has non-old-gen target" containment). The residual is the
  register-invisibility / running-thread transient-root gap tracked by A4 and
  `dohead-jit-heap-corruption-register-invisibility.md` — NOT the fixed
  deposit-cache staleness.

Use this repro (not the Spring classpath) to iterate on the residual: it needs
no suite classpath and fails in under ~2 minutes when it fails.
