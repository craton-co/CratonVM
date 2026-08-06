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
