# PublicSuffixList failed to cast to itself in every Spring Boot test that forks the classpath

Status: **FIXED (2026-09-05)**. All four classes pass on CratonVM with the JIT
on and off; HotSpot passes them too, on the same harness.

## The symptom

Four classes, one stack frame, JIT-on only:

```
java.lang.ClassCastException: class org.apache.hc.client5.http.psl.PublicSuffixList
  cannot be cast to class org.apache.hc.client5.http.psl.PublicSuffixList
	at org.apache.hc.client5.http.psl.PublicSuffixMatcher.<init>(PublicSuffixMatcher.java:88)
```

- `org.springframework.boot.restclient.autoconfigure.RestClientObservationAutoConfigurationWithoutMetricsTests`
- `org.springframework.boot.restclient.autoconfigure.RestTemplateObservationAutoConfigurationWithoutMetricsTests`
- `org.springframework.boot.security.autoconfigure.web.servlet.SecurityFilterAutoConfigurationEarlyInitializationTests`
- `org.springframework.boot.servlet.autoconfigure.MultipartAutoConfigurationTests`

All four use Spring Boot's `@ClassPathExclusions` / `@ForkedClassPath`, whose
`ModifiedClassPathClassLoader` re-defines the application classpath in a child
loader whose parent is the PLATFORM loader — so the child defines its own copy
of every application class, including `httpclient5`'s.

## The missing half of the original write-up: which ENGINE refused

The page this record replaces recorded the message and stopped. The message
itself said which engine: CratonVM's interpreter builds a
`ClassCastException` through `throw_runtime_error`, which appends HotSpot's
module/loader parenthetical (`(X is in unnamed module of loader …)`); the JIT
cast helper (`vm/src/jit/helpers.rs`) builds its own throwable and does not.
The recorded message has no parenthetical. It was a compiled `checkcast`.

`-Jit off` confirmed it directly: **all four classes pass under `--nojit` on
the same binary**, and `CRATONVM_DBG_TYPECHECK_FILTER=PublicSuffixList` named
the two sides:

```
[DBG_TYPECHECK] site-recorded: target_name=org/apache/hc/client5/http/psl/PublicSuffixList
  recv=…/PublicSuffixList (id=3125 loader=Application)
  resolved_target=…/PublicSuffixList (id=3126 loader=Some(UserDefined(3)))
[DBG_TYPECHECK] REFUSED-by-recorded-site: …
```

Two live copies of one class, the receiver from the application loader and the
site's compile-time target from the forked child — a shape CratonVM's flat
class store produces routinely once a test forks the classpath.

## Root cause: a JIT/interpreter divergence, and a fix applied at one of two paths

The interpreter ACCEPTS a same-named class from another loader. Both
`op_checkcast` and `op_instanceof` end their assignability chain in
`loader_aware_name_assignable`, and `CRATONVM_LOADER_AWARE_RESOLUTION` is
default ON (`classloading/src/class_manager.rs` is the single source of
truth for that gate; a stale comment in
`vm/src/runtime/interpreter/constants.rs` still says it is off).

`jit_typecheck_resolve` has the same fallback — `is_assignable_to_name`, added
for `SpringBootContextLoaderAotTests` (Residual 6) with a comment that names
the divergence exactly: *"the id-based checks above wrongly refuse a cast the
interpreter's loader-faithful CP resolution would pass"*. But that fallback
sits at the BOTTOM of the function, after the by-name path, and the
recorded-site branch near the top returns `false` before ever reaching it. A
site whose target class was already loaded when the method was compiled —
which is every site in this cluster — never saw the fix.

**The fix** gives the recorded-site branch the same loader-duplication
fallback, immediately before its refusal, with its own
`recorded-site-loader-duplication-by-name` trace stage. Same predicate, same
rule, both paths — the "a fix that lands for a subset is at the wrong level"
shape.

## Evidence

Binary: release build of dev `a740427a` plus this fix, `cratonvm-psl4`,
Azure host 2. Suite runner: `apps/spring-boot-suite-runner`.

| arm | before | after |
|---|---|---|
| CratonVM, JIT on | 4 FAIL | **4 PASS** |
| CratonVM, `--nojit` | 4 PASS | 4 PASS |
| HotSpot jdk-25, same harness | 4 PASS | 4 PASS |

The HotSpot arm is the cross-check the original page listed as missing: all
four pass on the reference VM, so all four were genuine CratonVM defects, not
fixture gaps.

## What the original page claimed, and how each claim came out

* *"Not GC-relocation-dependent"* — held. It is not a GC bug at all.
* *"Not a duplicate jar on the classpath"* — held, and it is the point: the
  second copy of `PublicSuffixList` is created by Spring Boot's own child
  loader, from the SAME jar.
* *"Working hypothesis: something in CratonVM's classloader delegation does
  not keep `PublicSuffixList` scoped to one loader"* — **wrong in its
  direction**. Two copies existing is expected once a test forks the
  classpath; HotSpot has two copies as well. What was wrong is that the
  compiled `checkcast` refused a cast the interpreter accepts.
* *"Whether only `PublicSuffixList` is affected is untested"* — answered: the
  class is incidental. Any compiled `checkcast` whose recorded site target and
  receiver are two loader copies of one name refused, which is why four
  unrelated autoconfiguration tests shared one frame.
* *"Not cross-checked against HotSpot"* — done, above.

## Repro (for a future regression)

```bash
cd apps/spring-boot-suite-runner
CV_BIN=<binary> pwsh -File ./run-spring-boot-suite.ps1 -Category all \
  -ClassList <(printf 'module\tclass\n%s\t%s\n' \
    module/spring-boot-servlet \
    org.springframework.boot.servlet.autoconfigure.MultipartAutoConfigurationTests)
```

`CRATONVM_DBG_TYPECHECK_FILTER=PublicSuffixList` prints the receiver and the
recorded target with their loaders; a refusal reads `REFUSED-by-recorded-site`
and an accepted loader duplicate reads
`recorded-site-loader-duplication-by-name`.
