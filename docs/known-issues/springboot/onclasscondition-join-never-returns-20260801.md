# `OnClassCondition`'s second thread never finishes, and `main` waits in `Thread.join` forever

**Status: OPEN.** Signature captured with a watchdog stack dump; cause not
located. Filed because it is what stopped
`BasicErrorControllerIntegrationTests` from producing 14 *consecutive* clean
runs on 2026-08-01 — the class itself is green (see
[`../../internal/fixed-suite-bugs/springboot/basicerrorcontroller-jit-only-failure-20260731-FIXED.md`](../../internal/fixed-suite-bugs/springboot/basicerrorcontroller-jit-only-failure-20260731-FIXED.md))
and this is a separate defect that was in the way.

## Symptom

The process stops making progress between two per-test Spring Boot context
boots and never resumes. No exception, no output, no exit.
`--stack-dump-on-timeout 1500` names the wait site exactly:

```
tid=0 os_tid=3449799 name="main" alive=true daemon=false blocked=true roots=973
  top=java/lang/Thread.join@129
   <- java/lang/Thread.join@2
   <- org/springframework/boot/autoconfigure/condition/OnClassCondition$ThreadedOutcomesResolver.resolveOutcomes@4
```

and the 90-odd frames below it are an ordinary Spring Boot startup:

```
BasicErrorControllerIntegrationTests.load
  SpringApplication.run → refreshContext → refresh
    AbstractApplicationContext.invokeBeanFactoryPostProcessors
      ConfigurationClassPostProcessor.processConfigBeanDefinitions
        ConfigurationClassParser$DeferredImportSelectorGrouping.getImports
          AutoConfigurationImportSelector.getAutoConfigurationEntry
            OnClassCondition$ThreadedOutcomesResolver.resolveOutcomes
              Thread.join()
```

`OnClassCondition` splits Spring Boot's auto-configuration class-presence
filtering across TWO threads — the caller evaluates one half, a spawned thread
evaluates the other, and the caller `join()`s it. The spawned thread never
completes, so `join()` never returns.

Two occurrences captured this way (2026-08-01), both at the same wait site.

## What is and is not established

**Established.** The wait site, and that the process is otherwise idle.

**NOT established.** What the spawned thread is doing. The dump prints frames
for the wait-site thread only; every other entry in the 226-thread summary is
`top=<no-frame-trace>`, and the summary's `alive=` / `blocked=` columns are not
maintained (every thread but `main` reads `alive=false blocked=true
state="<unset>"`, including threads that must have been live). **Do not read
`alive=false` on the joined thread as "the thread is dead and the join missed
its wakeup"** — that field cannot support the claim. Extending the watchdog to
dump every registered thread's frames is the obvious next step and would
probably settle this in one occurrence.

## Rate

Same class, same host and fixture, 3 concurrent, 2026-08-01:

| binary | runs | stalls |
|---|---|---|
| pristine `origin/dev` `5443fae920` | 34 | **0** |
| the `fix/basicerrorcontroller-jit-20260801` branch, before merging dev | 20 | 2 |
| the same branch merged with dev | 54 | 1 |

**Not load-gated, contrary to a first reading.** The first two occurrences
happened while the shared 16-core host was carrying an external load average
above 100, which invited the conclusion that contention was the trigger. The
third happened at load ~22, on an otherwise quiet box. Scheduling pressure may
change the odds; it is not the mechanism.

Three events in 74 runs of the branch against **0** in 34 of pristine dev
**does not distinguish the two trees**: at a ~4% rate a 34-run control comes up
empty about a quarter of the time. A same-binary A/B of the branch's only
JIT-churn change at the time (an optimizing-tier code-buffer retry, since
removed, 20 interleaved on/off pairs) came back 20/20 clean on BOTH arms, so
that change was not the trigger either. Anyone tempted to blame — or clear — a specific change on these
numbers should collect a much larger control first.

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

Not reliably reproducible on demand — three occurrences in 54 runs.

```bash
D=/data/data/spring-boot-tomcat-crossmodule-20260717
CP="$(cat $D/module/spring-boot-webmvc/build/cratonvm-test-cp.txt):$D/sb-runner"
CLS=org.springframework.boot.webmvc.autoconfigure.error.BasicErrorControllerIntegrationTests
cd "$D" && <cratonvm> --stack-dump-on-timeout 1500 -Xmx2g -cp "$CP" SbRunner "$CLS"
```

Always arm `--stack-dump-on-timeout`; the first occurrence was killed by the
harness with no dump and cost the information.

## Affected classes

- `module/spring-boot-webmvc` — `org.springframework.boot.webmvc.autoconfigure.error.BasicErrorControllerIntegrationTests`. Nothing about the stall is specific to this class: `OnClassCondition` runs on every Spring Boot context boot, and this class boots one per test, which is simply a lot of chances.
