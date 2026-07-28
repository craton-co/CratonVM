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

## 2026-07-03 (later) — re-tested against `fix/interp-local-liveness` (dev `7b662188`): residual UNCHANGED, not the same bug

Dev landed a real, separate, verified fix in this window
(`abca7a25` + `c224e1a9`, "interpreter retention imprecision RESOLVED",
part of the G1 parallel-evac persistent-forwarding root-remap fix) for an *unbounded-retention* interpreter
root-scan gap: `lstore`/`dstore` left the cat-2 reservation slot `i+1`
holding a stale object reference, keeping it a GC root forever, plus a new
per-bci liveness filter (`runtime/local_liveness.rs`,
`CRATONVM_NO_LOCAL_LIVENESS=1` kill switch). Given the resemblance to this
repro's own "lost-tag interpreter local" lead, it was worth re-testing
before assuming Family-A was still open.

**Result: still open, and NOT resolved by that fix.** Full before/after
distribution (Windows binary `throttlefix-v4.exe` = throttle fix +
`7b662188`, 5 runs each, 90 s cap):

| build | corruption-warning counts (5 runs) | outcome |
|---|---|---|
| pre-liveness (`throttlefix-v2`/`v3`) | 0, 0, 0 (repeated 90–300 s probes) | rc=124 every time, but CLEAN of the corruption signature in this window |
| post-liveness (`throttlefix-v4`), liveness ON | 518, 17, 578, 1517, 183 | rc=124 every time, corruption present |
| post-liveness (`throttlefix-v4`), `CRATONVM_NO_LOCAL_LIVENESS=1` | 140, 0, 109 (+ 0, 27 from an earlier pair) | rc=124 every time, corruption STILL present, just less frequent |

The kill switch does not zero the corruption (only reduces its rate ~5–7×),
so `local_liveness.rs` is **not the source** — it changes GC cadence/timing
enough to surface a pre-existing race far more often. Cross-checking the
dev range that actually separates the "0" and "nonzero" builds
(`37b185be..62f1f39a`, the same range that introduced the unrelated
`Optional.orElse` JUnit-launcher regression) shows substantial *concurrent
GC / DoHead* work landing in the same window — `8e64d9a5`/`d57ad7a0` "DoHead
comb-7 SIGSEGV" (added the `mark_young: rejecting object ... implausible
extent` diagnostic seen in the v4 hangwalk logs) and `57f545be` "fix three
live-object-freeing races in the concurrent old-gen cycle". The corruption
signature itself is unchanged (`kind=Object but array_length=512
(num_slots=5, class_id=…)`) — the same JIT inline-alloc header-write /
GC-root race already tracked as OPEN in
`dohead-jit-heap-corruption-register-invisibility.md`. **Conclusion: the
"0 corruption" reading on pre-liveness builds was a timing artifact (the
race is rare enough that short probe windows sometimes miss it), not
evidence the bug was fixed or absent there — the new DoHead diagnostics
report the SAME pre-existing race more reliably/loudly, not a new one.**
Family-A / the register-invisibility residual remains OPEN; the fix
delivered in this session (`ec951634`, the deposit-path JIT-scan-cache
invalidation) is unrelated and unaffected — the real Spring test remains
6/6 green throughout every build referenced above.

A genuinely new, distinct diagnostic surfaced in this pass and is worth a
future session's attention: `mark_young: rejecting object at <addr> with
implausible extent 0 (kind=0, array_len=N, num_slots=0)` recurring at the
**same fixed address**, with `array_len` incrementing by exactly 1 on each
successive occurrence (observed 512→513→514 across 3 GC cycles at
`0x17beae91e00`). That monotonic-per-cycle pattern is NOT random corruption
noise — it looks like a live `int` counter field (plausibly
`ConcurrencyThrottleSupport`/`Throttle.count`, incremented once per
`beforeAccess`) being misread through a header-shaped lens at a reused young
address. Worth a follow-up with `CRATONVM_DBG_CELLCORRUPT` (added in the
same dev window specifically "to identify the holder of a corrupt Value
cell") pointed at this exact repro.

Use this repro (not the Spring classpath) to iterate on the residual: it needs
no suite classpath and fails in under ~2 minutes when it fails.
