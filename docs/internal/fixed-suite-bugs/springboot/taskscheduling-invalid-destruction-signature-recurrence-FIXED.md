# `TaskSchedulingAutoConfigurationTests` "Invalid destruction signature" — recurs despite the RESOLVED disposable-bean-adapter doc using this exact class as its "closed" repro

**Status: FIXED — 2026-07-17.** This recurrence was the same `Class.getMethods()` override-shadowing defect. Its fix is documented and regression-tested in [`class-getmethods-override-shadowing-duplicate-close-cluster-FIXED.md`](class-getmethods-override-shadowing-duplicate-close-cluster-FIXED.md).

## Symptom

```
=> java.lang.AssertionError:
Expecting:
 <Unstarted application context org.springframework.boot.test.context.assertj.AssertableApplicationContext[startupFailure=org.springframework.beans.factory.BeanCreationException]>
not to have any beans of type:
 <org.springframework.scheduling.TaskScheduler>:
but context failed to start:
 org.springframework.beans.factory.BeanCreationException: Error creating bean with name 'customScheduledExecutorService' defined in org.springframework.boot.autoconfigure.task.TaskSchedulingAutoConfigurationTests$ScheduledExecutorServiceConfiguration: Invalid destruction signature
 	at org.springframework.core.NestedRuntimeException.<init>(NestedRuntimeException.java:45)
 	at org.springframework.beans.BeansException.<init>(BeansException.java:41)
 	at org.springframework.beans.FatalBeanException.<init>(FatalBeanException.java:35)
 	at org.springframework.beans.factory.BeanCreationException.<init>(BeanCreationException.java:96)
```

`tests=16 failed=1`. Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/core_spring-boot-autoconfigure.org.springframework.boot.autoconfigure.task.TaskSchedulingAutoC-8cf107ed74e2.out.log`

## Discrepancy with an existing RESOLVED doc

[`docs/internal/fixed-suite-bugs/spring-disposablebeanadapter-invalid-destruction-signature-cluster-RESOLVED.md`](../spring-disposablebeanadapter-invalid-destruction-signature-cluster-RESOLVED.md)
covers the **identical error text and bean name shape** ("Invalid
destruction signature", `AbstractAutowireCapableBeanFactory.doCreateBean`'s
generic destroy-method-resolution catch handler), across a 34-class cluster
that explicitly names `TaskSchedulingAutoConfigurationTests` as "the
smallest repro in the cluster — use it first" (its own "Next step" section).
That doc's status is:

> **Status: RESOLVED (stale current-dev report; verified 2026-07-12).** ...
> A direct probe constructed the real Spring Framework 7.0.7
> `DisposableBeanAdapter` around a concrete `AutoCloseable` bean ... It
> completed successfully under the uniquely built current-dev binary ...
> The original 34-class grouping was made from Spring's catch-all outer
> message, without a captured inner exception or a distinct remaining
> failure mode. The current exact adapter-path evidence closes that unpinned
> cluster; no separate residual was identified in this note to keep open.

That resolution was reached via a *standalone* probe (a hand-built
`DisposableBeanAdapter` + `AutoCloseable`), not by re-running
`TaskSchedulingAutoConfigurationTests` itself — and five days later, in this
2026-07-17 rerun, that exact class fails with the exact same error text
again, for a bean of a type (`ScheduledExecutorService`, via
`customScheduledExecutorService`) whose relevant destroy-method-inference
shape (a public no-arg `close()`/`shutdown()` inherited from a JDK
interface) is squarely the family the RESOLVED doc's own "Spring-side
mechanism" section describes as the likely trigger. Per this session's
triage instructions, this is noted explicitly as a genuine discrepancy
rather than either (a) silently re-filed as an unrelated fresh bug, or (b)
silently treated as a known-closed duplicate and dropped.

Two explanations are equally plausible from the evidence available this
session (neither confirmed):
- The RESOLVED doc's standalone probe tested a materially different code
  shape (plain `AutoCloseable.close()`) than
  `TaskSchedulingAutoConfigurationTests`'s actual failing bean
  (`ScheduledExecutorService`, whose relevant destroy method is
  `shutdown()`/`close()` inherited via a different, more deeply-nested JDK
  interface hierarchy — `ScheduledExecutorService` extends
  `ExecutorService` extends `Executor`, and only `ExecutorService` declares
  `shutdown()`), so the probe never actually exercised this specific
  reflective-resolution path and the 2026-07-12 closure was premature for
  this particular bean shape even if correct for the literal case it
  tested.
- A regression was reintroduced between 2026-07-12 and 2026-07-17 in the
  same reflective destroy-method-resolution area, independent of whatever
  the standalone probe verified.

No new investigation into which of these is correct was done this session
(the underlying reflective exception is still hidden behind Spring's
catch-all `"Invalid destruction signature"` wrapper in this log, exactly as
the RESOLVED doc's own "Spring-side mechanism" section describes — a deeper
capture with `e.getCause()` instrumentation is still needed to see the real
underlying exception CratonVM throws).

**Cross-reference (2026-07-17, same-day parallel triage):** a sibling
investigation this session captured the un-wrapped inner exception for 4
other classes hitting this same outer text
(`disposablebeanadapter-getmethods-hierarchy-duplicate-destroy-method-cluster-FIXED.md`)
— in every one of those, the `.out.log`'s full `Caused by:` chain (not
visible in `.err.log`) reveals `BeanDefinitionValidationException: Could
not find unique destroy method ... N candidates`, and that doc pins a
concrete source-level mechanism in
`native-builtins/src/lang_class.rs::collect_public_methods` (backing
`Class.getMethods()`, which does not deduplicate an overridden method
across hierarchy levels the way HotSpot does). This class's
`customScheduledExecutorService` bean (`ScheduledExecutorService`, destroy
method `shutdown`) is a textbook case for that same mechanism —
`shutdown()` is declared on `ExecutorService` and any concrete
implementation returned by `Executors.newScheduledThreadPool(...)`, which
is exactly the "override declared at N hierarchy levels" shape the other
doc's fix direction targets. This class's own `.out.log` should be checked
for the same `Caused by:` chain to confirm before assuming this is
identical (not done in this session, this doc's log excerpt above only
shows the outer wrapper) — first choice explanation above (probe tested a
different shape than this bean) is now the stronger of the two.

## Affected classes

| Module | Class |
|---|---|
| `core/spring-boot-autoconfigure` | `org.springframework.boot.autoconfigure.task.TaskSchedulingAutoConfigurationTests` |
