# `OnClassCondition`'s second thread never finishes, and `main` waits in `Thread.join` forever

**Status: OPEN.** Signature captured with a watchdog stack dump; cause not
located. Filed because it is what stopped
`BasicErrorControllerIntegrationTests` from producing 14 consecutive clean runs
on 2026-08-01 — the class itself is green (see
[`../../internal/springboot/basicerrorcontroller-jit-only-failure-20260731.md`](../../internal/springboot/basicerrorcontroller-jit-only-failure-20260731.md)),
and this is a separate, load-triggered defect that was in the way.

## Symptom

The process stops making progress mid-run, between two per-test Spring Boot
context boots, and never resumes. No exception, no output, no exit.
`--stack-dump-on-timeout 1500` names the wait site exactly:

```
tid=0 os_tid=3449799 name="main" alive=true daemon=false blocked=true roots=973
  top=java/lang/Thread.join@129
   <- java/lang/Thread.join@2
   <- org/springframework/boot/autoconfigure/condition/OnClassCondition$ThreadedOutcomesResolver.resolveOutcomes@4
```

and the 93-frame dump below it is an ordinary Spring Boot startup:

```
BasicErrorControllerIntegrationTests.load
  SpringApplication.run → refreshContext → refresh
    AbstractApplicationContext.invokeBeanFactoryPostProcessors
      ConfigurationClassPostProcessor.processConfigBeanDefinitions
        ConfigurationClassParser$DeferredImportSelectorGrouping.getImports
          AutoConfigurationImportSelector.getAutoConfigurationEntry
            AutoConfigurationImportSelector$ConfigurationClassFilter.filter
              OnClassCondition$ThreadedOutcomesResolver.resolveOutcomes
                Thread.join()
```

`OnClassCondition` splits Spring Boot's auto-configuration class-presence
filtering across TWO threads — the caller evaluates one half, a spawned thread
evaluates the other, and the caller `join()`s it. The spawned thread never
completes, so `join()` never returns.

## What is and is not established

**Established.** The wait site, and that the process is otherwise idle. The
watchdog aborts after dumping, so the run ends `EXIT=134` (SIGABRT).

**NOT established.** What the spawned thread is doing. The dump prints frames
for the wait-site thread only; every other entry in the 226-thread summary is
`top=<no-frame-trace>`, and the summary's `alive=` / `blocked=` columns are not
maintained (every thread but `main` reads `alive=false blocked=true
state="<unset>"`, including threads that must have been live). **Do not read
`alive=false` on the joined thread as "the thread is dead and the join missed
its wakeup"** — that field cannot support the claim. Extending the watchdog to
dump every registered thread's frames is the obvious next step and would
probably settle this in one occurrence.

## Rate and trigger

14 runs of `BasicErrorControllerIntegrationTests` on 2026-08-01, Linux
x86-64, `cratonvm-becit-fix5-20260801`, 2 concurrent:

| outcome | runs |
|---|---|
| PASS 26/26 | 11 |
| stalled, killed at the harness's 2400 s cap (no watchdog armed yet) | 1 |
| stalled, watchdog dump above, `EXIT=134` | 1 |
| `failed=1` — client-side `HttpClient request timed out` on `testRequestBodyValidationForMachineClient` | 1 |

Both stalls happened while the shared 16-core host was carrying an external
load average above 100 (peaks of 266 from other tenants); all eleven clean runs
happened below ~60. Three subsequent replacement runs at load ~40 are recorded
in the companion doc. The correlation is strong enough to call this
load-triggered and weak enough that it is NOT an explanation: a scheduling
delay does not by itself make a `join` never return.

## Where to look first

`OnClassCondition`'s two-thread filtering already produced one confirmed
CratonVM defect in the same shape — the main thread and the resolver thread
race to load the same classes through an isolated loader, and
`ucl_try_define_local_class` (`native-builtins/src/classloader.rs`) probed
`find_loaded_class_for_loader` before taking its per-`(loader, name)` define
lock and never re-probed inside it. That one surfaced as
`NoClassDefFoundError`, was fixed 2026-07-27, and is written up in
[`../../internal/fixed-suite-bugs/springboot/data-redis-urlclassloader-uncached-classpath-hang-FIXED.md`](../../internal/fixed-suite-bugs/springboot/data-redis-urlclassloader-uncached-classpath-hang-FIXED.md)
(item 4). A *hang* in the same two-thread window is the natural sibling: the
resolver thread parked on a define lock whose holder never clears
`in_progress`, or the two threads each holding what the other waits for.

Second candidate, independent of classloading: the thread-termination
notification. If a thread can finish without waking a `join`er, this shape
follows directly. `Thread.join(long)` is `while (isAlive()) wait(0)` under the
thread object's monitor, so the exit path must publish liveness and
`notifyAll` **under that same monitor** — a notify that does not take it can
slip between the `isAlive()` check and the `wait`.

## Reproduction

Not reliably reproducible on demand. It appeared twice in fourteen runs of
this class, only under heavy external load:

```bash
D=/data/data/spring-boot-tomcat-crossmodule-20260717
CP="$(cat $D/module/spring-boot-webmvc/build/cratonvm-test-cp.txt):$D/sb-runner"
CLS=org.springframework.boot.webmvc.autoconfigure.error.BasicErrorControllerIntegrationTests
cd "$D" && <cratonvm> --stack-dump-on-timeout 1500 -Xmx2g -cp "$CP" SbRunner "$CLS"
```

Always arm `--stack-dump-on-timeout`; the first occurrence was killed by the
harness with no dump and cost the information.

## Affected classes

- `module/spring-boot-webmvc` — `org.springframework.boot.webmvc.autoconfigure.error.BasicErrorControllerIntegrationTests` (2 stalls in 14 runs). Nothing about the stall is specific to this class: `OnClassCondition` runs on every Spring Boot context boot, and this class boots one per test.
