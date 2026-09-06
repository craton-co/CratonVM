# PublicSuffixList failed to cast to itself in every Spring test that forks the classpath

Status: **FIXED (2026-09-05)**. Seven classes across TWO Spring codebases --
four in Spring Boot and three in Spring Framework, reached through THREE
different child-classloader mechanisms -- pass on CratonVM with the JIT on and
off. HotSpot passes them too, on the same harness.

## The symptom

One frame, two projects, two unrelated forked-classloader mechanisms, JIT-on
only:

```
java.lang.ClassCastException: class org.apache.hc.client5.http.psl.PublicSuffixList
  cannot be cast to class org.apache.hc.client5.http.psl.PublicSuffixList
	at org.apache.hc.client5.http.psl.PublicSuffixMatcher.<init>(PublicSuffixMatcher.java:88)
```

**Spring Boot** (`@ClassPathExclusions` / `@ForkedClassPath` →
`ModifiedClassPathClassLoader`):

- `org.springframework.boot.restclient.autoconfigure.RestClientObservationAutoConfigurationWithoutMetricsTests`
- `org.springframework.boot.restclient.autoconfigure.RestTemplateObservationAutoConfigurationWithoutMetricsTests`
- `org.springframework.boot.security.autoconfigure.web.servlet.SecurityFilterAutoConfigurationEarlyInitializationTests`
- `org.springframework.boot.servlet.autoconfigure.MultipartAutoConfigurationTests`

**Spring Framework** (`@CompileWithForkedClassLoader` →
`CompileWithForkedClassLoaderClassLoader`), added to the original page by a
second lane on 2026-09-05 and confirmed on all three collectors:

- `org.springframework.web.service.registry.HttpServiceProxyRegistrationAotProcessorTests`
- `org.springframework.web.service.registry.ImportHttpServiceRegistrarTests`

Both loaders have the same shape, and the second lane read it out of Spring
Framework's source: `super(testClassLoader.getParent())` — the child's parent
is the app loader's PARENT, so the child defines its OWN copy of every
application class it is asked for. **Two `Class` objects for one name is
working-as-designed, on HotSpot too.** That was the half the original page had
backwards: it treated the duplication itself as the defect and went looking
for a classloader-delegation gap.

## Root cause: a JIT/interpreter divergence, and a fix applied at one of two paths

The message itself said which engine threw it. CratonVM's interpreter builds a
`ClassCastException` through `throw_runtime_error`, which appends HotSpot's
module/loader parenthetical (`(X is in unnamed module of loader …)`); the JIT
cast helper (`vm/src/jit/helpers.rs`) builds its own throwable and does not.
The recorded message has no parenthetical. `-Jit off` confirmed it directly:
**every one of the six classes passes under `--nojit` on the same binary.**

`CRATONVM_DBG_TYPECHECK_FILTER=PublicSuffixList` named the two sides:

```
[DBG_TYPECHECK] site-recorded: target_name=org/apache/hc/client5/http/psl/PublicSuffixList
  recv=…/PublicSuffixList (id=3125 loader=Application)
  resolved_target=…/PublicSuffixList (id=3126 loader=Some(UserDefined(3)))
[DBG_TYPECHECK] REFUSED-by-recorded-site: …
```

The interpreter ACCEPTS a same-named class from another loader: both
`op_checkcast` and `op_instanceof` end their assignability chain in
`loader_aware_name_assignable`, and `CRATONVM_LOADER_AWARE_RESOLUTION` is
default ON (`classloading/src/class_manager.rs` is the single source of truth
for that gate; a stale comment in `vm/src/runtime/interpreter/constants.rs`
still says it is off).

`jit_typecheck_resolve` has the same fallback — `is_assignable_to_name`, added
for `SpringBootContextLoaderAotTests` (Residual 6) with a comment naming the
divergence exactly: *"the id-based checks above wrongly refuse a cast the
interpreter's loader-faithful CP resolution would pass"*. But that fallback
sits at the BOTTOM of the function, and the recorded-site branch near the top
returns `false` before reaching it. A site whose target class was already
loaded when the method was compiled — every site in this cluster — never saw
the fix.

**The fix** gives the recorded-site branch the same loader-duplication
fallback, immediately before its refusal, with its own
`recorded-site-loader-duplication-by-name` trace stage. Same predicate, same
rule, both paths.

## Evidence

Binary: release build of dev `a044e1fe1` merged with this fix, `cratonvm-psl4`,
Azure host 2. Runners: `apps/spring-boot-suite-runner`, `apps/spring-suite-runner`.

| arm | before | after |
|---|---|---|
| Spring Boot, JIT on | 4 FAIL | **4 PASS** |
| Spring Boot, `--nojit` | 4 PASS | 4 PASS |
| Spring Framework, JIT on | 2 FAIL | **2 PASS** (10/10 methods) |
| HotSpot jdk-25, same harness | PASS | PASS |

The Spring Framework arm is also the causal check, not just an outcome:
re-running it with `CRATONVM_DBG_TYPECHECK_FILTER=PublicSuffixList` shows the
compiled cast taking the NEW path —

```
1 DBG_TYPECHECK] enter
1 DBG_TYPECHECK] site-recorded
1 DBG_TYPECHECK] recorded-site-loader-duplication-by-name
```

— i.e. the exact site that used to print `REFUSED-by-recorded-site` is the one
the fix now admits. One code change, two independently-implemented forked
loaders, six classes.

The HotSpot arm is the cross-check the original page listed as missing: all
six pass on the reference VM, so all six were genuine CratonVM defects, not
fixture gaps.

## A seventh class, and the shape generalises past `PublicSuffixList`

`org.springframework.test.context.aot.TestContextAotGeneratorIntegrationTests`
had its own page the same day
(`known-issues/spring/qdox-parser-static-table-race-under-concurrent-first-use-20260905`),
recording `TypeDef cannot be cast to TypeDef` and reading it as a
static-publication race, with a caveat that it was *not confirmed* to share a
root cause with this one. It did: 4/4 methods pass on the fixed binary, twice,
and `CRATONVM_DBG_TYPECHECK_FILTER=TypeDef` shows the run taking
`recorded-site-loader-duplication-by-name` exactly once.

That class does NOT use `@CompileWithForkedClassLoader`, which is what the
other page's reasoning turned on — it gets its second loader copy from
`TestCompiler`/`DynamicClassLoader` instead. So the trigger is not a
particular test annotation: it is any child loader that defines its own copy
of a class a compiled `checkcast` also names. Three different mechanisms in
two codebases, one defect. (That page stays OPEN for the separate concurrent
static-publication gap its own probe measured, which this fix does not touch.)

## What the original page claimed, and how each claim came out

* *"Not GC-relocation-dependent"* — held. It is not a GC bug at all.
* *"Not a duplicate jar on the classpath"* — held, and it is the point: the
  second copy is created by the test framework's own child loader, from the
  SAME jar.
* *"Root cause is a CratonVM classloader-delegation gap"* (both the original
  Spring Boot framing and the broadened cross-project one) — **wrong in its
  direction**. The duplication is correct behaviour and HotSpot produces it
  too. What was wrong is that the compiled `checkcast` refused a cast the
  interpreter accepts.
* *"Whether only `PublicSuffixList` is affected is untested"* — answered: the
  class is incidental. Any compiled `checkcast` whose recorded site target and
  receiver are two loader copies of one name refused, which is why six
  unrelated tests in two codebases shared one frame.
* *"Not cross-checked against HotSpot"* — done, above.
* *"Still stops short of a pinned CratonVM source line"* — pinned:
  `jit_typecheck_resolve`'s recorded-site branch in `vm/src/jit/helpers.rs`.

## Repro (for a future regression)

```bash
# Spring Boot
cd apps/spring-boot-suite-runner
CV_BIN=<binary> pwsh -File ./run-spring-boot-suite.ps1 -Category all \
  -ClassList <(printf 'module\tclass\n%s\t%s\n' \
    module/spring-boot-servlet \
    org.springframework.boot.servlet.autoconfigure.MultipartAutoConfigurationTests)

# Spring Framework
cd apps/spring-suite-runner
CRATONVM_BIN=<binary> ./run-suite.sh run --list <(printf '%s\t%s\n' \
  <spring-framework>/spring-web \
  org.springframework.web.service.registry.ImportHttpServiceRegistrarTests)
```

`CRATONVM_DBG_TYPECHECK_FILTER=PublicSuffixList` prints the receiver and the
recorded target with their loaders; a refusal reads `REFUSED-by-recorded-site`
and an accepted loader duplicate reads
`recorded-site-loader-duplication-by-name`.
