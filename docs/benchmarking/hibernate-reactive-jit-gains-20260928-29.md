# Hibernate Reactive Suite: JIT Gain Measurement — 2026-09-28/29

## Summary

Six-arm benchmark measuring JIT compilation gains on the Hibernate Reactive integration
test suite (205 classes, PostgreSQL), run serially one arm at a time.

Two CratonVM binaries were tested:

- **20260928 run**: commit `6d39e8dcc15c` (pre-interpreter-wave-42)
- **20260929 rerun**: commit `134a197a39a6` (tip of dev, "interpreter round i1 wave 42")

## HotSpot results (20260928 binary)

```
Arm                VM         JIT Mode   PASS  FAIL  NOTEST  Total  Wall(s)  SumClassMs  Gain vs No-JIT
---------------------------------------------------------------------------------------------------------
hotspot-nojit      HotSpot    no-jit     203   0     2       205    237.0    232,277     1.00x (baseline)
hotspot-1c         HotSpot    1c-only    203   0     2       205     54.0     50,854     4.39x
hotspot-default    HotSpot    default    203   0     2       205     45.0     43,396     5.27x (1.20x vs C1)
```

HotSpot: no-jit → C1 = **4.39x**, C1 → default = **1.20x**, total = **5.27x**

## CratonVM results — old binary (20260928, commit 6d39e8dcc15c)

```
Arm                VM         JIT Mode   PASS  FAIL  NOTEST  Total  Wall(s)  SumClassMs  Gain vs No-JIT
---------------------------------------------------------------------------------------------------------
cratonvm-nojit     CratonVM   no-jit     202   1     2       205    699.3    688,596     1.00x (baseline)
cratonvm-1c        CratonVM   1c-only    202   1     2       205    809.5    794,602     0.86x  ← slower
cratonvm-default   CratonVM   default    202   1     2       205    757.4    750,823     0.92x  ← slower
```

JIT added overhead on this workload — compilation cost was not amortized over many
short-lived test classes. C1 was 0.86x and full JIT was 0.92x vs interpreter.

The single FAIL on all CratonVM arms: `CachedQueryResultsTest` (4 test methods) —
identical across nojit/1c/default, confirming it is not a JIT issue. HotSpot passes it.

## CratonVM results — tip of dev (20260929, commit 134a197a39a6)

Binary: `/data/wt-hib-jitgain-20260929/target/release/cratonvm-hib-jitgain-20260929`
Build time: 8m09s

```
Arm                 PASS  FAIL  ABRT  NOTEST  Classes  Wall(s)  SumClassMs  Gain vs NoJIT
------------------------------------------------------------------------------------------
cratonvm-nojit      1137     4     0       6      205    877.6     869,909          1.00x
cratonvm-1c         1137     4     0       6      205    835.6     828,467          1.05x
cratonvm-default    1137     4     0       6      205    789.7     780,582          1.11x
------------------------------------------------------------------------------------------

CratonVM 1c vs no-jit:   1.05x  (877.6s → 835.6s)
CratonVM def vs 1c:      1.06x  (835.6s → 789.7s)
CratonVM total JIT gain: 1.11x  (877.6s → 789.7s)
```

> [!NOTE]
> PASS counts here are individual test *methods* (via `@@RESULT ok=N` summed across all
> classes). The old run counted classes (PASS=202 classes). Both runs cover 205 classes.

**JIT is now net positive**: 1.11x total gain (was 0.92x on the 20260928 binary — a
regression). The 169 commits between the two runs improved JIT enough to overcome the
compilation overhead on this short-lived I/O-bound workload.

FAIL=4 is consistent across all three arms → not JIT-related. NOTEST=6 classes had no
discoverable tests.

## Delta between the two CratonVM binaries

```
Metric          Old (20260928)   New (20260929)   Change
Wall nojit      699.3s           877.6s           +178s   (interpreter slower — more correctness work)
Wall 1c         809.5s           835.6s           +26s
Wall default    757.4s           789.7s           +32s
JIT gain total  0.92x            1.11x            ← now positive
```

The interpreter is slower in absolute terms (new wave of correctness fixes), but the
JIT gain ratio improved because the JIT overhead grew more slowly than the interpreter
overhead did.

## Setup

- **Host**: Azure `20.80.105.49`
- **JDK**: `/data/toolchain/jdk-25`
- **Test suite**: `/data/cratonvm/apps/hibernate-reactive-suite-runner/`
- **Test list**: `testlist-nohang.txt` (205 classes; `MultithreadedInsertionWithLazyConnectionTest` excluded due to hang history)
- **Database**: PostgreSQL 15 (`local-postgres` Docker container, port 5432)
- **Runner script**: `run_6runs_jit_gains.py` (HotSpot + old CratonVM), `run_3arms_cratonvm_20260929.py` (new CratonVM tip)
- **Results**: `results_jit_gains_6runs_20260928/` and `results_jit_gains_cv3arms_20260929/`
