# `AotIntegrationTests` now HANGS where it used to fail in 20 minutes

| | |
|---|---|
| **Status** | **OPEN.** The failure mode changed from FAIL to HANG. Both are wrong against HotSpot, but a hang costs a machine for hours, so read this before running the class. |
| **Scope** | `org.springframework.test.context.aot.AotIntegrationTests` (spring-framework, `spring-test`). |
| **Oracle** | HotSpot `found=4 succ=2 fail=0 skip=2`, 63 s. |
| **Cap your timeout.** | It will not finish. 90 min is enough to observe everything below; 5.5 h buys nothing. |

## What changed

`de3c4d35b` / `5d265dbeb` fixed `Collections.unmodifiableList(x).get(i)`
throwing `ArrayIndexOutOfBoundsException` on a **valid** index. That fix is
correct and independently verified (see
`docs/known-issues/repros/list-get-oob/UM.java`), and it takes
`BeanRegistrationsAotContributionTests` from 8/6 to **14/14 = HotSpot**.

It also changed this class's failure mode:

| binary | collections fix | nested tests started | outcome |
|---|---|---|---|
| pre-fix | no | 4 | `found=4 succ=0 fail=2 skip=2` in **1209 s** |
| post-fix | yes | 2 | **hang**, killed at 10754 s |
| post-fix + data-slot redesign | yes | 2 | **hang**, identical |

## Why this is most likely UNMASKING, not a new defect

Before the fix, `CompileWithForkedClassLoaderExtension.runTest:140` —

```java
if (summary.getTotalFailureCount() > 0)
    throw summary.getFailures().get(0).getException();
```

— threw `AIOOBE` out of `.get(0)` itself and aborted the test method early. With
`get` working, execution proceeds into a phase it had never reached, and that
phase spins. This is the same shape as the CharBuffer `address` fix uncovering
the javac CCE beneath it: a correct fix exposing the next defect down.

**Not proven.** The alternative — that the fix itself introduced the loop — has
not been ruled out, and the obvious mechanism for it was ruled out (below). Do
not repeat the ruling-out; do the `--nojit` run.

## What has been ruled out

* **Not the per-`get` cost.** The first fix used `al_state`'s SIZE as the
  readability signal, which is 0 for an unreadable backing, so the fallback
  fired on every `get` — a VM dispatch per element. `de3c4d35b` switched to the
  DATA slot and removed the extra dispatch entirely. **The hang is identical
  either way**, so that was not it.
* **Not host contention.** The 10754 s run overlapped a 2.9 h Bean run; the
  re-run had the box to itself and hung the same way at the same line.
* **Not a deadlock.** One core is saturated throughout (CPU advanced 31 s per
  30 s of wall clock). It is a spin, not a wait.

## The one hard clue: the loop is in COMPILED code

`--stack-sample-ms 15000` produced **zero** `T19.H1 stack dump` records over the
whole run. That flag samples the interpreter dispatch loop, and its own help
text says JIT-compiled frames never reach it. Zero samples with a core pinned
means the loop is in JIT-compiled code.

Corroborating: RSS collapses to **10 MB** (from 157 MB) and stays flat — a tight
loop touching almost nothing.

## Where it stops

Identical in every post-fix run, at the second nested TestNG suite:

```
FINE [...LoggingListener] alter: [[Suite: "Command line suite" ...]]
FINE [...LoggingListener] onStart:  org.testng.TestRunner@…
FINE [...LoggingListener] onFinish: org.testng.TestRunner@…     <- stops here
```

with **no** `onTestStart` between them, where the pre-fix run ran a full
`BasicTestNGTests` / `BasicSpringTestNGTests` sequence at the same point
(`onTestStart` 4 pre-fix vs 2 post-fix). So the nested suite is now discovering
nothing and then spinning, rather than spinning mid-test.

## Next step

Run it with `--nojit`. That is the one arm that turns the sampler back on for
this loop:

```bash
KRUN_STACK=1 <beanall.sh> cvm aotnojit \
  org.springframework.test.context.aot.AotIntegrationTests --nojit --stack-sample-ms 15000
```

Expect it to be much slower — budget accordingly, and cap the timeout. If the
loop disappears under `--nojit`, it is a JIT defect and
`docs/known-issues/…/jit-*` is the right neighbourhood; if it persists, the
sampler will finally name the Java frame.

---

## 2026-08-06, later: the hang is gone, and what is under it is now named

Re-measured on `dev` @ `0bcbe2032` + `docs/internal/list-out-of-range-accessors-returned-null-FIXED-20260806.md`,
Azure `20.83.144.174`, real JDK 25, repaired classpath (0 of 254 missing).
**Do not budget 90 minutes any more — this is a 49-second failure.**

### Run the two methods separately

The class's two live methods differ by an order of magnitude in cost, and only
one of them was ever the problem. `apps/spring-suite-runner/onem.sh` runs a
single method with `one.sh`'s classpath and JVM args:

```bash
CRATONVM_BIN=<bin> ./onem.sh org.springframework.test.context.aot.AotIntegrationTests endToEndTests
```

| method | this binary | HotSpot |
|---|---|---|
| `endToEndTests` | **`found=1 succ=1 fail=0`**, 73-76 s | agrees |
| `endToEndTestsForBeanOverrides` | `found=1 succ=0 fail=1`, **49 s** | passes |

That is the whole reason to stop using the class as the oracle: a 900 s+
whole-class run that spends nearly all of its time in one method, on a shared
box that OOM-killed four consecutive acceptance attempts here.

### The remaining failure, and where it is

```
FAIL endToEndTestsForBeanOverrides() :: java.lang.IllegalArgumentException: array element type mismatch
	at org.springframework.core.annotation.TypeMappedAnnotation.adapt(TypeMappedAnnotation.java:487)
	at ...AbstractMergedAnnotation.getValue(AbstractMergedAnnotation.java:177)
	at ...SynthesizedMergedAnnotationInvocationHandler.invoke(...:76)
	at ...ContextLoaderUtils.resolveContextHierarchyAttributes(ContextLoaderUtils.java:138)
	at ...TestContextAotGenerator.processAheadOfTime(TestContextAotGenerator.java:181)
```

`TypeMappedAnnotation.java:487` is `Array.set(array, i, annotations[i].synthesize())`,
filling a `ContextConfiguration[]` with synthesized annotation proxies.

`Array.set`'s refusal now names both sides under `CRATONVM_DBG=coerce`
(added with this note; the message itself stays HotSpot's bare wording because
source witnesses pin it):

```
[DBG_COERCE] Array.set: rejecting -- array=org/springframework/test/context/ContextConfiguration
    component=org/springframework/test/context/ContextConfiguration (cid=ClassId(2547))
    value_class=jdk/proxy3/$Proxy27 (cid=ClassId(2552))
```

Exactly one rejection per run. So `is_subclass($Proxy27, ContextConfiguration)`
is false for a proxy that was created *from* that annotation type.

**This is almost certainly not a real type error.** It is the shape recorded in
`field-set-argument-type-mismatch-is-a-loader-split-use-dbg-coerce`: one class
NAME with two `ClassId`s under two loaders. `jdk/proxy3/` — not `jdk/proxy1/` —
says this proxy was defined for a third loader, and this class runs everything
under `@CompileWithForkedClassLoader`. Corroborating: a standalone
`Array.set(Ann[], Proxy.newProxyInstance(cl, {Ann.class}, h))`
(`repro/ArraySetProxyRepro.java`) is byte-identical to HotSpot, proxies
included — so `Array.set` and `is_subclass` handle proxies correctly when there
is only one loader in play.

### Next step

Print the proxy's recorded interfaces and the loader id of both `ClassId`s at
the rejection, then compare with `--dump-*` for the two `ContextConfiguration`
entries. If they are two ids for one name, the defect is in how the forked
loader's parent delegation is modelled, not in `Array.set` —
`user-loader-parent-chain-was-unmodelled-rust-side` and
`forked-loader-mixed-copies-triage-recipe` are the right neighbourhood, and
`Array.set` is only where it happens to surface.

### Two things ruled out — do not repeat them

* **Not the young-GC livelock**, however much `jcmd GC.heap_info` looks like it.
  On the older binary that did hang, it read `Young 1024.0 MB / 1.0 GB (100.0%
  used)` with `Old 135.9 MB (6.6%)`, frozen byte-for-byte across eight minutes —
  the exact signature in `young-gc-trigger-livelock-under-nonmoving-sweep`. It
  was not that: `CRATONVM_DBG_YOUNG_TRIGGER=1` reported
  `free_list=1002MB live=21MB threshold=921MB`, i.e. a young generation with
  1 GB free. The 100 % is a high-water cursor the non-moving sweep never
  retreats, and the GOOD control binary reaches 100 % too and then finishes.
* **Not the per-`get` cost, confirmed independently.** A build carrying the
  variant of the collections fix that *does* pay a `size()` dispatch per `get`
  hung identically to one that does not, on the same host, same method.
