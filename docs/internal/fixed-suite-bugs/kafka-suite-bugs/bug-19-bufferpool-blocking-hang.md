# Bug 19 — `BufferPoolTest` hang — ROOT CAUSE: JDK-version mismatch at boot (NOT a VM defect)

**Status: RESOLVED (environmental).** On a quiet machine with a matching JDK boot
image, the hang does not reproduce. The earlier diagnoses in this doc (synthetic
`Condition` lost-wakeup; GC-quiescence deadlock) were **both wrong** — artifacts of
(a) CPU starvation from leftover `cratonvm.exe` processes and (b) running CratonVM
against a **too-old JDK boot image (JDK 17)** while the workload expected JDK 19+.

## TL;DR

`BufferPoolTest` — like almost all of `java.util.concurrent` — creates worker
threads with a **`Runnable` target** (`new Thread(runnable)` / thread pools), then
blocks waiting for one of them to `signal`/`notify`/`unpark`. The hang is simply:
**a `Runnable`-target thread never runs its target**, so the wakeup never comes.

That happens **only when CratonVM boots from a JDK older than 19.** JDK ≥19 stores a
Thread's `Runnable` in `Thread.holder.task` (`Thread$FieldHolder`, a class that does
not exist before JDK 19). When CratonVM boots a JDK-17 `java.base`:
- `Thread$FieldHolder` is absent → `ClassNotFoundException`,
- CratonVM's real-JDK `Thread` layout is mismodelled (reflection surfaces only the
  `name` field),
- so `holder.task` can never be populated, and the native `Thread.run()`
  (`native-builtins/src/lib.rs`, the `("java/lang/Thread","run","()V")` handler)
  finds no task and **silently returns** — the target Runnable never executes.

Insidiously, the VM still reports `java.version=25.0.1` even while booting JDK 17.

## Fix

Boot CratonVM from a JDK **≥ 19** (ideally the same JDK the tests are compiled
against — here JDK 25). The launcher picks its boot image from
`--java-home` › `CRATONVM_JAVA_HOME` › `JAVA_HOME` › `java` on `PATH`. On this box
`JAVA_HOME` was stale (`temurin17-jdk`) while `PATH` had JDK 25, so the default
boot was JDK 17. Either:

```
cratonvm --java-home "C:\Program Files\Eclipse Adoptium\jdk-25.0.2.10-hotspot" -cp . <Class>
# or set CRATONVM_JAVA_HOME / fix JAVA_HOME to a JDK >= 19
```

**No VM code change is required** — verified by reverting all experimental code
edits and re-running stock: stock VM + JDK 25 passes the entire matrix below.

## Evidence (clean machine: 0 contending `cratonvm.exe`)

Minimal standalone repros in [`repro19/`](repro19/) (all PASS on HotSpot):

| repro | exercises | JDK17 boot | JDK25 boot (stock) |
|-------|-----------|:---------:|:------------------:|
| `R0Sub` | subclass `run()` vs `new Thread(runnable)` | subclass ✅ / target ❌ | both ✅ |
| `R0Group` | `new Thread(group, runnable, name)` | ❌ | ✅ |
| `R0Basic` | plain thread body + volatile handoff | ❌ | ✅ |
| `R2WaitNotify` | intrinsic `wait()`/`notify()` cross-thread | HANG | PASS |
| `R3Park` | `LockSupport.park()`/`unpark()` cross-thread | HANG | PASS |
| `R1Condition` | `ReentrantLock`+`Condition` (BufferPool shape) | HANG | PASS |

Key discriminator: a `Thread` *subclass overriding `run()`* always worked (dispatch
starts at the subclass, never reads `holder.task`); only the `Runnable`-target form
failed — which is exactly what `java.util.concurrent` uses everywhere. The
`R2/R3/R1` matrix hung identically across **all** configs (default JIT, `--nojit`,
`CRATONVM_REAL_AQS=1`) because the defect is upstream of every blocking primitive:
the thread that would wake the waiter never ran.

Boot-JDK proof: under `--java-home <JDK17>` `Class.forName("java.lang.Thread$FieldHolder")`
→ `ClassNotFoundException` and `Thread.class` reflects only `name`; under
`--java-home <JDK25>` all 19 Thread fields incl. `holder:FieldHolder` appear,
`FieldHolder` loads, `holder.task` is populated, and `CHILD RAN` prints.

## Connection to bug-23

This is the **same root cause** as the genuine-hang members of
[bug-23](bug-23-timeout-hang-family.md) — notably `AbstractCoordinatorTest`
(`Object.wait` leaf). Any test that waits on a notify/signal/unpark from a
`Runnable`-target worker hangs under a <19 boot JDK. (The throughput-bound bug-23
members — Mockito/ByteBuddy mock-gen, JUnit reflective discovery — are unrelated and
remain a separate performance matter.)

## Latent footgun (recommended hardening, separate from this bug)

Running CratonVM against a boot JDK older than the workload's class-file version
mismodels `java.lang.Thread` and makes `new Thread(runnable)` a silent no-op with
**no diagnostic** — while `java.version` still reports the newer release. Worth a
startup warning when the boot `java.base` release is older than expected, or when
`Thread$FieldHolder` fails to resolve in real-JDK mode.

## Affected classes (reclassified: all clear under JDK ≥19 boot)
- producer.internals.BufferPoolTest — was TIMEOUT under JDK17 boot; not a VM defect.
