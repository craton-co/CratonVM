# `OnClassCondition.addAll` NPE-cast-to-`String[]` cluster: root cause found, fix verified correct, but BLOCKED from merging by a newly-exposed heap-corruption bug

**Status: OPEN (blocked). The original 75-class/348-occurrence
`ClassCastException` cluster (formerly tracked as
`onclasscondition-npe-cast-to-string-array-cluster.md`, now retired to
`docs/internal/` — this doc supersedes it) is fully root-caused and a fix is
verified to eliminate every occurrence. That fix cannot ship: applying it
makes 3 Spring Boot test classes crash/hang with a heap-corruption defect
that does not reproduce on the unmodified baseline. The corruption's root
cause is NOT understood. Do not merge
`fix/onclasscondition-npe-array-alias-20260712` until it is.**

## The original bug — ROOT-CAUSED, FIX CONFIRMED CORRECT

Across essentially every Spring Boot autoconfigure module (`jackson`,
`jersey`, `tomcat`, `cloudfoundry`, `jdbc`, `grpc`, `task`, …), 75 classes /
348 occurrences hit the identical error:

```
java.lang.ClassCastException: java.lang.NullPointerException cannot be cast to [Ljava.lang.String;
	at org.springframework.boot.autoconfigure.condition.OnClassCondition.addAll(OnClassCondition.java:133)
	at org.springframework.boot.autoconfigure.condition.OnClassCondition.getCandidates(OnClassCondition.java:124)
	at org.springframework.boot.autoconfigure.condition.OnClassCondition.getMatchOutcome(OnClassCondition.java:90)
```

wrapped by Spring as `IllegalStateException: Error processing condition on
<SomeAutoConfiguration>` → `BeanDefinitionStoreException` → `Unstarted
application context`, which is why this single root cause fanned out into
several different-looking note-column buckets across ~150-200 of the original
508-FAIL suite run.

**Root cause** (confirmed via `CRATONVM_IAE_TRACE=1` + direct log inspection,
not guessed): `native-builtins/src/lang_class.rs`'s
`annotation_element_to_java_typed`, `AnnotationElementValue::Class(desc)`
arm. When a `Class`-typed annotation element's class can't be resolved and
no `container_loader` is involved (i.e. **not** the classloader-isolation
scenario — the overwhelmingly common case for plain
`@ConditionalOnClass(SomeOptionalClass.class)`), the code returned a bare
Java `null` (`Value::Object(None)`) instead of the `TypeNotPresentException`
sentinel that the rare, narrow `container_loader`-present branch already
built.

Real HotSpot (`AnnotationParser.parseClassValue`) always defers an
unresolvable `Class`-typed annotation element to a `TypeNotPresentException`
(`ExceptionProxy`), never a bare `null`. Spring's own annotation-attribute
extraction code is specifically written to catch that exact exception type —
it's the whole mechanism `@ConditionalOnClass` relies on to detect "optional
dependency absent." Handing back `null` instead let Spring's
`TypeMappedAnnotation`/`MergedAnnotation` `classValuesAsString` conversion do
`null.getName()` — a genuine `NullPointerException` that Spring's
`TypeNotPresentException`-aware handling doesn't recognize, which gets
mis-stashed as the attribute's "value" instead of propagating, surfacing as
the `ClassCastException` above.

**Fix**: build the same `TypeNotPresentException` sentinel in the
no-`container_loader` path too. For a `Class[]`-declared member (e.g.
`@ConditionalOnClass`'s `value`, which is `Class<?>[]` — the common case),
collapse the whole member to the sentinel the moment any element is
unresolvable, matching HotSpot's `AnnotationInvocationHandler.invoke`
(throws for the whole accessor call on the first bad `Class[]` element,
never hands back an array with the sentinel embedded).

**Verified**: eliminates the target `ClassCastException` from the standalone
single-class repro (`CloudFoundryInfoEndpointWebExtensionTests`) and from
**all 75/75** originally-affected classes — zero remaining occurrences of
`"cannot be cast to [Ljava.lang.String"` anywhere in a full re-run of the
originally-affected class list.

Fix lives on branch `fix/onclasscondition-npe-array-alias-20260712`
(worktree `C:\craton\CratonVM-onclasscondition-npe-array-20260712`, branched
from `origin/dev` at `e48480a99`), commit `819e01048`. **Not merged.**

## The blocker — a newly-exposed heap-corruption bug, root cause NOT understood

Applying the fix makes 3 classes crash/hang where they don't on the
unmodified baseline:
- `JdbcSessionAutoConfigurationTests` (module `spring-boot-session-jdbc`)
- `BraveAutoConfigurationTests` (module `spring-boot-micrometer-tracing-brave`)
- `SecurityAutoConfigurationTests` (module `spring-boot-security`)

100% reproducible, single class, `-Parallel 1` (no shared-host contention
involved). Symptom: a heap-corruption guard fires first —

```
gen_heap::get_field: out-of-bounds field read dropped (undersized object layout
  — class declares more fields than the object was allocated with)
  obj=0x1cc...178 index=0 num_slots=0 class_id=ClassId(6) class_name=java/lang/String real_field_count=Some(4)
gen_heap::read_slot: corrupt Value cell (out-of-range discriminant) — HIB-CV-32
```

— a `java/lang/String`-classed object found allocated with 0 slots instead
of 4, detected a few seconds into a normal test run (not during JDK
bootstrap; confirmed via `CRATONVM_IAE_TRACE` that this happens during
ordinary `@ConditionalOnClass(...)` processing on real
`@Configuration`-class annotations, e.g. `DataSourceConfiguration$Hikari`).
Under the default JIT-on mode this culminates in a native `SIGSEGV` with 3
`external/jit` frames in the backtrace, same registers/addresses every time.

**5 independent fix variants tried, all produced the byte-for-byte identical
crash** (ruling out each hypothesis in turn):
1. Per-access array-element scan in `annotation_proxy_dispatch_impl`
   (`vm/src/vm/vm_exec.rs`) — reverted entirely (confirmed zero diff vs
   `origin/dev` for that file); removing it didn't help.
2. Construction-time array-collapse without stack-trace capture in the
   sentinel — same crash. Rules out stack-walking.
3. Same, but always completing the per-element loop instead of early-
   returning with a partially-filled array — same crash. Rules out
   "orphaned half-filled array."
4. Properly `pin_native_root`/`read_native_pin`/`unpin_native_roots`
   -protecting the exception/message/cause objects inside
   `make_type_not_present_exception` (which now allocates far more often
   than the rare classloader-isolation call site it originally served) —
   same crash.
5. Most thorough: a permanently-`add_global_root`-rooted singleton sentinel
   (eliminates per-occurrence allocation after the first-ever call), PLUS
   `pin_native_root`-protecting the per-element array in the `Class[]`-arm
   loop, PLUS the same for `names_arr`/`values_arr` in
   `create_annotation_proxy`'s outer per-element loop — **still the
   identical crash**.

**`--nojit` bisection (2026-07-13): JIT is NOT the cause.** Re-ran the same 3
classes with `-Jit off` against the variant-5 (most thorough) fix binary.
The identical corruption-guard signature appeared in all 3 classes under
pure interpretation too, just with different observable failure modes:
`JdbcSessionAutoConfigurationTests` hangs (300s timeout) instead of
crashing; `BraveAutoConfigurationTests` still dies (process terminates
mid-output, `rc=1`, but WITHOUT the usual fatal-error banner this time —
whatever kills it under `--nojit` bypasses the normal crash handler);
`SecurityAutoConfigurationTests` merely fails cleanly (corruption present
but happened to be "dropped" harmlessly by the guard). This rules out
JIT-compiled exception-dispatch codegen as the cause — the `external/jit`
frames in the JIT-mode crashes were incidental (JIT is on by default and
heavily active during real Spring Boot startup), not causal.

**Leading remaining hypothesis, untested**: `add_global_root`/
`resolve_global_root` (the mechanism backing the variant-5 singleton) is
comparatively rarely-exercised elsewhere in this codebase (mainly JNI global
refs, parked async-I/O attachments) — unlike `pin_native_root`, which is
heavily exercised and well-trodden. It's plausible this specific mechanism
has its own latent bug that the annotation code is the first thing to hit at
this frequency. Cheap next experiment: revert just the singleton-via-global-
root piece (go back to allocating a fresh, `pin_native_root`-protected
sentinel every time, accepting the perf cost) and see whether the corruption
changes character or disappears.

## Repro

```powershell
# Original ClassCastException cluster (fixed — verify with the fix binary):
apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 -SpringBootRoot C:\craton\CratonVM\apps\spring-boot `
  -ClassList <TSV row for module/spring-boot-jersey's JerseyAutoConfigurationDefaultFilterPathTests, or any of the other 74 affected classes> `
  -Start 1 -Count 1 -Exe <cratonvm exe built from fix/onclasscondition-npe-array-alias-20260712>

# Heap-corruption blocker (3-row TSV: module<TAB>class):
#   module/spring-boot-session-jdbc	org.springframework.boot.session.jdbc.autoconfigure.JdbcSessionAutoConfigurationTests
#   module/spring-boot-micrometer-tracing-brave	org.springframework.boot.micrometer.tracing.brave.autoconfigure.BraveAutoConfigurationTests
#   module/spring-boot-security	org.springframework.boot.security.autoconfigure.SecurityAutoConfigurationTests
apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 -SpringBootRoot C:\craton\CratonVM\apps\spring-boot `
  -ClassList <the 3-row TSV above> -Start 1 -Count 3 -Parallel 1 -Exe <fix binary>
# add `-Jit off` to reproduce under the interpreter too.
```

## Related

- [[reference_native_pending_return_stale_exception_override]] (FIXED
  `b90702a6`) — same general "wrong exception object aliasing a value slot"
  shape as the ORIGINAL bug here, different call site.
- SB-04 in [[reference_spring_boot_functional_suite]] (still OPEN) —
  possibly the same root-cause family; this doc's original bug gave that
  investigation a much more specific, standalone-reproducible entry point.
- HIB-CV-32 — the heap reference-integrity guard message referenced above;
  worth checking whether this corruption is a fresh manifestation of that
  same tracked defect class.
