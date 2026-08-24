# `CRATONVM_BG_COMPILE=0` nulls a reference argument — the bisect lever has its own wrong answer

**Status: OPEN CratonVM JIT correctness defect, in a NON-default configuration.**
With the background compiler off, `TechEmpowerTest` dies in ~9 s with
`NullPointerException: Cannot invoke "io.vertx.core.net.TcpConfig.getSendBufferSize()"
because "other" is null` — a copy constructor whose argument reads as `null`.
Reproduces 5/5 across two binaries; `--nojit` alongside it PASSES, so it is the
JIT. Found 2026-08-24 while A/B-ing something else.

## Why a non-default configuration is worth a page

`CRATONVM_BG_COMPILE=0` is not a user knob, it is a **bisect lever**: it is how
you ask "is this the background tier?" and it is reached for exactly when a
defect is already suspected. A lever that introduces its OWN wrong answer
answers that question falsely in both directions —

* a workload that fails only with the lever ON reads as "the background tier is
  innocent", when the lever may simply have replaced one failure with another;
* a workload that fails with the lever OFF reads as a reproduction, when it may
  be this.

This session lost a TechEmpower A/B to exactly that: the `BG_COMPILE=0` arm
"failed 4/4", which looked like a result until the log said the failure was a
different NPE, in a different class, before the workload had started.

## The measurement

`org.hibernate.reactive.techempower.TechEmpowerTest`, live Postgres via
Testcontainers, same classpath and args in every arm:

| binary | config | result | wall |
|---|---|---|---|
| `3a4dc5626` (`dev`) | `CRATONVM_BG_COMPILE=0` | **FAIL** | 9 s |
| `3a4dc5626` (`dev`) | `CRATONVM_BG_COMPILE=0` + `CRATONVM_DISABLE_JIT=1` | **PASS** | 94 s |
| `3a4dc5626` (`dev`) | default | PASS | 38–65 s |
| `95c210f37` (2026-08-22) | `CRATONVM_BG_COMPILE=0` | **FAIL** | 9 s |
| fix/osr-direct-bind-… | `CRATONVM_BG_COMPILE=0` | **FAIL** 2/2 | 9, 10 s |

So: not new (it is on the 08-22 binary too), not this branch's, and **the JIT**
— `--nojit` in the same configuration passes.

## What fails

```
java.lang.NullPointerException: Cannot invoke
  "io.vertx.core.net.TcpConfig.getSendBufferSize()" because "other" is null
	at io.vertx.core.net.TcpConfig.<init>(TcpConfig.java:42)
```

`TcpConfig(TcpConfig other)` is an ordinary copy constructor. Its `other`
parameter is non-null at every call site Vert.x has — the caller has just
constructed or fetched it — so a `null` there is a reference argument that did
not survive the call, not a program error. It happens during Vert.x server
setup, before the workload runs at all, which is why the run dies in 9 s.

This is the same *species* as several already-recorded defects — a reference
argument or field that reads as `null` in compiled code
(`getUncaughtExceptionHandler` returning null, the unpinned `String` argument
read after an allocation, the G30-1 silent reference-slot coercion). It has not
been attributed to any of them; that is the work.

## What the next session should do

1. **`CRATONVM_DBG_JITC=1` with `BG_COMPILE=0`** and find which compile door
   produced `TcpConfig.<init>` and its caller. With the background tier off,
   every compile is on the mutator, so the population is small and the door is
   not in question — what is in question is what changed about the ARGUMENT.
2. **Bisect the argument, not the callee.** The callee is a copy constructor
   with one reference parameter; log what the CALLER passes immediately before
   the call. If the caller's value is non-null there, the loss is in the call
   sequence (an unpinned argument across an allocation is the recorded shape,
   `a-native-may-hold-refs-but-not-across-a-callback`); if it is already null,
   the defect is upstream in whatever produced it.
3. **Do not assume it is specific to `BG_COMPILE=0`.** Turning the background
   tier off changes compile ORDER, and compile order is what decides which
   sites bind directly. The same defect may be reachable in the default
   configuration on a workload with different timing — which would make this a
   default-configuration defect that only this lever makes deterministic. That
   possibility is what makes it worth chasing rather than filing under "a
   diagnostic flag is broken".
4. The reproducer is ~9 s and deterministic (5/5), which is a far better
   instrument than most of this family gets.

## Reproduce

```bash
CRATONVM_BG_COMPILE=0 \
  <cratonvm> --java-home <jdk25> --Xmx 1500m @common.args \
  -Djunit.jupiter.execution.timeout.default=600s \
  -Dcraton.batch=1 CratonRunner org.hibernate.reactive.techempower.TechEmpowerTest

# the control that says it is the JIT
CRATONVM_BG_COMPILE=0 CRATONVM_DISABLE_JIT=1 … (same)
```

## Related

* `techempower-wrong-answer-was-the-indy-trap-FIXED-20260824.md` — the
  workload's OTHER defect, closed; this one is not it and the two must not be
  conflated (that page's arms all run with the background tier ON).
* `getuncaughtexceptionhandler-returned-null-…`, `a-symptom-that-renames-itself-…`
  — the same species, a reference that reads as null in compiled code.
