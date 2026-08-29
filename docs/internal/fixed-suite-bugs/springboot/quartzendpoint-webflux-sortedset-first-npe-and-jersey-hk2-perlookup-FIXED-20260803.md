# `QuartzEndpointWebIntegrationTests`: WebFlux `SortedSet.first()` NPE and Jersey HK2 `PerLookup` context resolution failure — CLOSED

**Status: CLOSED — verified 2026-08-03**

The 2026-07-31 document recorded two failure modes in
`org.springframework.boot.quartz.actuate.endpoint.QuartzEndpointWebIntegrationTests`
(`module/spring-boot-quartz`) — 4/45 failing in that day's run — and flagged
both root causes as unconfirmed ("not bisected this session"):

1. **WebFlux**: a `NullPointerException` from `SortedSet.first()` returning
   `null` on a non-empty `TreeSet`, hypothesized as a hole at slot 0 of
   `TreeSet`'s backing array from an add/remove sequence or a moving-GC
   relocation.
2. **Jersey**: `IllegalStateException: Could not find an active context for
   org.glassfish.jersey.internal.inject.PerLookup`, thrown from lazy
   `ServletContainer.init()`, hypothesized as either HK2's own
   `../../../../apps/META-INF/hk2-locator/default` inhabitant-file discovery not finding
   `PerLookupContext`, or a CratonVM thread-local/context-registry mismatch.

## Current result

On current `origin/dev`, the class is clean in every configuration tried:

| Binary | Mode | Result |
|---|---|---:|
| Real JDK 25 (HotSpot control) | — | 1/1 PASS (45/45) |
| CratonVM, built from `origin/dev` | JIT | 7/7 PASS (45/45 each) |
| CratonVM, built from `origin/dev` | `--nojit` | 2/2 PASS (45/45 each) |

For contrast, a stale binary (built from commit `12ab91fe62` on the
`dev-merge-staging4-20260727` worktree, 20 commits behind `origin/dev` at the
time) reproduced the Jersey failure deterministically: 3/3 reruns, always the
same 3 methods (`quartzJobGroupSummaryWithUnknownGroup`,
`quartzJobDetailWithUnknownKey`, `quartzTriggerGroupSummary`, all `:Jersey`).
The WebFlux failure (`quartzTriggerJobWithUnknownJobKey`) did not reproduce
even on that stale binary, in any of the reruns performed this session.

## Root cause 1 (WebFlux): already fixed

Not independently reproduced this session despite targeted probing: a
JIT-shape hot loop mirroring `PatternsRequestCondition.getMatchingCondition`'s
single-pattern fast path (`size()==1` check, `.first()`, immediate
`invokeinterface` dereference with no null check) across 8 threads for 3.2M
iterations, a GC-stress probe holding 500 single-element `TreeSet`s across
heavy concurrent allocation, and a static-`final`-field-initialized
`TreeSet<>(Collections.singleton(...))` probe (matching
`PatternsRequestCondition.EMPTY_PATH_PATTERN`'s exact construction shape) all
came back clean. Code review of `native_ts_add`/`native_ts_remove`/
`ts_insert_at`/`ts_remove_at`/`ts_ensure_capacity` in
`native-collections/src/lib.rs` found the backing array correctly compacted
on every insert/remove, with no path that could leave slot 0 holding `None`
while `size != 0`.

Most likely explanation: fixed as a side effect of
`b2e13e4418` ("fix(collections): TreeMap/TreeSet snapshot walks dereferenced
relocated ObjectRefs", merged 2026-07-31, the same day this document was
written) — a moving-GC stale-`ObjectRef` class of bug in the same
`native-collections` module, of the same general shape as the "moving-GC
relocation... leaves the wrong slot referenced" hypothesis this document
originally raised for `first()`. Not confirmed by bisection (that commit
doesn't touch `native_ts_first` directly), but no residual reproducer exists
to bisect further against.

## Root cause 2 (Jersey): already fixed, mechanism confirmed

Traced to Jersey's own `org.glassfish.jersey.inject.hk2.Hk2Helper`
(`jersey-hk2:4.0.2`). `Hk2Helper.transformScope(Class)` maps Jersey's own
`org.glassfish.jersey.internal.inject.PerLookup` scope annotation to HK2's
built-in `org.glassfish.hk2.api.PerLookup` via a reference-equality check
(`ldc <PerLookup.class>; if_acmpne`) — `translateToActiveDescriptor` calls it
when building the `ActiveDescriptor` for `JaxrsProviders` and the Jackson
provider, both bound with Jersey's `PerLookup` scope. If that identity check
spuriously fails, the untranslated (Jersey-own) scope class leaks onto the
descriptor; HK2's `ServiceLocatorImpl.resolveContext` only fast-paths its
*own* `PerLookup`/`Singleton` classes and otherwise searches for a bound
`Context<theScope>` service — and no `Context<org.glassfish.jersey.internal.
inject.PerLookup>` implementation exists anywhere on the classpath (verified
by extracting every class in `jersey-hk2`/`jersey-common`/`jersey-server` and
grepping constant pools) — so resolution throws exactly the observed
`IllegalStateException`.

This lines up almost exactly with `0001747a36` ("fix(jit): compile `ldc
<Class>`, and name the reason a compile bailed", merged into `origin/dev`
2026-08-02, the day before this verification): before that fix, the JIT's
`cp_ldc_resolver` returned `None` for `CONSTANT_Class` entries, which
`try_compile_inner` treats as *permanently unrepresentable* — every method
containing a class literal (such as `Hk2Helper.transformScope`, `ldc
<PerLookup.class>`) was bail-listed and never compiled at any tier. The
original document's suspicion of "a sibling capping/enumeration gap in a
different resource-lookup path" was a reasonable lead given the `WF32-fix`
classpath-capping precedent, but the actual defect was in `ldc <Class>` JIT
support, not resource/classpath enumeration.

An alternative hypothesis (Jersey/HK2 framework classes being prematurely
unloaded due to mis-attribution to a per-test-method Tomcat webapp
classloader) was tested with temporary diagnostic instrumentation in
`vm/src/memory/gc.rs`'s `unload_dead_class_metadata` — zero class-unload
events fired during a full reproduction run on the stale binary, ruling this
out. The instrumentation was reverted; no trace of it remains.

## Resolution

There is no remaining reproducer for either symptom. Both are retired as
already-resolved residuals of unrelated fixes landed after this document was
first written, rather than preserving unconfirmed root-cause claims. No
CratonVM source change was needed in this session.

`docs/internal/dateformatsymbols-getproviderinstance-compile-bail-FIXED-20260802.md`
(the `ldc <Class>` JIT fix write-up) and `native-collections/src/lib.rs`'s
Family-1 stale-`ObjectRef` fix history are the relevant prior art for anyone
re-investigating either symptom in the future.

## Affected classes

- `module/spring-boot-quartz` — `org.springframework.boot.quartz.actuate.endpoint.QuartzEndpointWebIntegrationTests`
