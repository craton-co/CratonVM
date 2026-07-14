# Tomcat endpoint executor `Thread.start()` sporadic `IllegalThreadStateException` — CLOSED

**Status: FIXED / retired on 2026-07-14.**

## Original symptom

The 2026-07-13 Windows Tomcat DoHead sweep saw four low-rate failures while
an endpoint executor prestarted a newly created worker:

```
java.lang.IllegalThreadStateException
  at org.apache.tomcat.util.threads.ThreadPoolExecutor.addWorker
  at org.apache.tomcat.util.threads.ThreadPoolExecutor.prestartAllCoreThreads
```

That operation is a single `Thread.start()` on a fresh Tomcat `TaskThread`.
It is not port contention or an application-executor native path.

## Resolution

Current `dev` already contains the needed real-JDK thread-construction repair
(commit `a24035f24`, which is an ancestor of `dev`). `Thread` and its freshly
allocated `Thread$FieldHolder` are pinned across the re-entrant holder
constructor call, then both references are reread from their native pins
before the holder is published. This prevents a moving collection from
publishing a stale holder whose `threadStatus` may be read as non-NEW.

The prior note was filed after that repair landed, so the historical single
failure cannot be attributed more precisely from its saved stack trace. The
current behavior is nevertheless directly covered at the exact API boundary:

- `vm/tests/resources/cratonvm/ThreadPoolExecutorPrestartProbe.java` creates
  a Tomcat-shaped `Thread` subclass and repeatedly prestarts it through a real
  JDK `ThreadPoolExecutor`.
- `vm/tests/threadpoolexecutor_prestart_regression.rs` runs 2,000 such
  prestarts under CratonVM whenever a test binary is available.

## Validation

Using the uniquely built
`C:\tmp\cratonvm-threadpool-prestart-20260714-target\release\cratonvm.exe`:

| Runtime | Probe | Result |
| --- | --- | --- |
| HotSpot | 10,000 Tomcat-shaped worker prestarts | `PRESTART_OK` |
| CratonVM `--nojit` | 2,000 worker prestarts | `PRESTART_OK` |
| CratonVM JIT | 20,000 worker prestarts | `PRESTART_OK` |
| Focused Rust regression | 2,000 worker prestarts | 1 passed |

The JIT validation is four times the reported approximate 1-in-5,000 failure
interval and produced no `IllegalThreadStateException`. The historical Tomcat
runner directories (`apps/tomcat-suite-runner` and `apps/tomcat`) are not
present in this `dev` checkout, so this direct real-JDK executor probe is the
reproducible replacement for the unavailable suite fixture.

No residual `Thread.start()` / fresh-worker failure remains; this document is
therefore archived under `docs/internal`.
