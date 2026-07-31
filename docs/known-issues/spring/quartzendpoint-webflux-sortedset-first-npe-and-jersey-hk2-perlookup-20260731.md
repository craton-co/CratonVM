# `QuartzEndpointWebIntegrationTests`: WebFlux `SortedSet.first()` NPE and Jersey HK2 `PerLookup` context resolution failure

**Status: OPEN — found 2026-07-31**

## Symptom

`module/spring-boot-quartz` — `org.springframework.boot.quartz.actuate.endpoint.QuartzEndpointWebIntegrationTests`
failed 4/45 in the 2026-07-31 hang-reverify run (1500s timeout, completed in
738.7s — not a timeout):

```
SBRUNNER_RESULT tests=45 failed=4 aborted=0 skipped=0 containersFailed=0
```

All 4 failures are HTTP 500s from a parameterized test (run once per embedded
web-server flavor); the same test methods pass under the other flavors in
this same run, so this is not a total-class failure:

- `quartzTriggerJobWithUnknownJobKey` **(WebFlux only)**: expected 404, got 500.
- `quartzJobGroupSummaryWithUnknownGroup` **(Jersey only)**: expected 404, got 500.
- `quartzJobDetailWithUnknownKey` **(Jersey only)**: expected 404, got 500.
- `quartzTriggerGroupSummary` **(Jersey only)**: expected 200, got 500.

Full logs:
- `apps/spring-boot-suite-runner/.suite/results/craton-hangverify-20260731/all-jit/logs/module_spring-boot-quartz.org.springframework.boot.quartz.actuate.endpoint.QuartzEndp-e3de43e38499.out.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-hangverify-20260731/all-jit/logs/module_spring-boot-quartz.org.springframework.boot.quartz.actuate.endpoint.QuartzEndp-e3de43e38499.err.log` (silent — no VM warnings/crashes; both defects surface as ordinary Java exceptions server-side)

Not a regression: `docs/internal/fixed-suite-bugs/springboot/disposablebeanadapter-getmethods-hierarchy-duplicate-destroy-method-cluster-FIXED.md`
lists this exact class at 45/45 failing with a *different* symptom
(`BeanCreationException: ... Invalid destruction signature` from
`Class.getMethods()` returning duplicate `Scheduler.shutdown` candidates),
fixed 2026-07-17. That failure text does not appear anywhere in today's run
(all 41 passing tests, including bean startup/shutdown, are clean), so that
fix is holding. `docs/internal/fixed-suite-bugs/springboot/quartzautoconfigurationtests-jdbc-jobstore-not-applied-FIXED.md`
is about `QuartzAutoConfigurationTests`, a different class, and does not
mention either symptom below — a false positive by name similarity, not
relevant here.

## Root cause 1 (WebFlux): `SortedSet.first()` returns Java `null` on a non-empty `TreeSet`

```
21:08:57.417 [reactor-http-nio-3] ERROR o.s.web.server.adapter.HttpWebHandlerAdapter -- [b85a7a16-2] 500 Server Error for HTTP POST "/actuator/quartz/jobs/samples/does-not-exist"
java.lang.NullPointerException: Cannot invoke "org.springframework.web.util.pattern.PathPattern.matches(org.springframework.http.server.PathContainer)" because the return value of "java.util.SortedSet.first()" is null
	at org.springframework.web.reactive.result.condition.PatternsRequestCondition.getMatchingCondition(PatternsRequestCondition.java:162)
```

Spring's `PatternsRequestCondition.getMatchingCondition` calls
`.first()` on its own `TreeSet<PathPattern>` of configured patterns (a
single-pattern fast path) and immediately calls `.matches(...)` on the
result. `SortedSet.first()`/`TreeSet.first()` is contractually never allowed
to return `null` for a non-empty set (it throws `NoSuchElementException`
only when empty) — real HotSpot cannot produce this NPE.

CratonVM's implementation, `native_ts_first` in `native-collections/src/lib.rs`
(~line 35203), throws `NoSuchElementException` correctly when the backing
array is `None` or `size == 0`, but otherwise unconditionally returns
`ctx.get_array_element(data, 0)` with no check that slot 0 actually holds a
live element:

```rust
fn native_ts_first(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    ...
    let data = match data_opt {
        Some(d) if size != 0 => d,
        _ => return Err(... NoSuchElementException ...),
    };
    Ok(Some(ctx.get_array_element(data, 0)))
}
```

If the backing array's logical slot 0 can ever hold a stale/hole `null`
while `size != 0` (e.g. after an add/remove sequence that does not keep the
array's front compacted, or after a moving-GC relocation of the backing
array leaves the wrong slot referenced), `first()` silently hands back Java
`null` instead of a real element or an exception — exactly the observed
shape. Not bisected against a standalone `TreeSet` add/remove/first probe
this session (no rebuild/run performed); flagged as the grounded read of the
function that implements `first()`, not independently confirmed live.

## Root cause 2 (Jersey): HK2 `PerLookup` context resolution fails on first request

```
MultiException stack 1 of 6
java.lang.IllegalStateException: Could not find an active context for org.glassfish.jersey.internal.inject.PerLookup
	at org.jvnet.hk2.internal.ServiceLocatorImpl._resolveContext(ServiceLocatorImpl.java:2244)
	...
	at org.glassfish.jersey.server.ApplicationHandler.initialize(ApplicationHandler.java:315)
	at org.glassfish.jersey.servlet.WebComponent.<init>(WebComponent.java:339)
	at org.glassfish.jersey.servlet.ServletContainer.init(ServletContainer.java:151)
	...
	at org.apache.catalina.core.StandardWrapper.initServlet(StandardWrapper.java:819)
```

is thrown from lazy `ServletContainer.init()` on the Jersey servlet's first
allocation, and recurs (6 stacks in one `MultiException`, all the same
"could not find an active context"/"error occurred while locating the
context" shape) while Jersey's HK2 `ServiceLocator` tries to resolve its
built-in `PerLookup`-scoped providers (`JaxrsProviders`, the Jackson
`DefaultJacksonJaxbJsonProvider`, etc.). `PerLookupContext` is a core HK2
context that real HK2 always has active; failing to find it points at
either (a) HK2's own `META-INF/hk2-locator/default` inhabitant-file
discovery not finding/registering `PerLookupContext`, or (b) a CratonVM
thread-local/context-registry mismatch during Jersey's servlet
initialization.

Not bisected this session. The known `WF32-fix` classpath-capping mechanism
(`native-builtins/src/classloader.rs:4512`, "capping N flat-classpath
matches ... CratonVM module-jar flood") was checked and ruled out as the
direct cause — it only bounds `META-INF/MANIFEST.MF` enumeration, not the
`META-INF/hk2-locator/`/`META-INF/services/` resources HK2 actually scans —
but a sibling capping/enumeration gap in a different resource-lookup path is
a plausible lead worth checking first, since the `.err.log` is otherwise
silent (no crash, just the ordinary Java-level `MultiException`).

## Suggested next steps

1. Standalone `TreeSet<T>` probe: add N elements, remove some, call
   `first()`/compare against real JDK to determine whether the backing array
   can end up with a live `size` but a hole at slot 0 — narrows root cause 1.
2. Standalone HK2 `ServiceLocator` probe (or a minimal Jersey
   `ServletContainer` bring-up) to see whether `PerLookupContext` is
   discovered via `getResources("META-INF/hk2-locator/default")` under
   CratonVM vs. real JDK — narrows root cause 2.

## Affected classes

- `module/spring-boot-quartz` — `org.springframework.boot.quartz.actuate.endpoint.QuartzEndpointWebIntegrationTests`
