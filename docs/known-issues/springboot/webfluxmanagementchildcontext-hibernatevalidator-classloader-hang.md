# `WebFluxManagementChildContextConfigurationIntegrationTests`: HANG in Hibernate Validator classloader resource lookup (new `dev` regression, unconfirmed root cause)

**Status: OPEN — found 2026-07-19; broader than webflux, confirmed 2026-07-20**

## Update 2026-07-20 — 4 more affected classes, CPU-sampled (busy, not parked), pre-existing on unmodified `dev`

Found while fixing
[`capturedoutput-empty-console-cluster.md`](capturedoutput-empty-console-cluster.md)
(archived — see
`../../internal/springboot/capturedoutput-empty-console-cluster-FIXED.md`):
running `core/spring-boot`'s `ConfigurationPropertiesTests` (114 tests) as a
full class — not just its one CapturedOutput-affected method — hangs the
same way, and 3 more classes in unrelated modules do too:
`core/spring-boot`'s `org.springframework.boot.context.properties.bind.BinderTests`,
`module/spring-boot-webmvc`'s
`org.springframework.boot.webmvc.autoconfigure.WebMvcObservationAutoConfigurationTests`
(HANG as a full class, despite its 2 individually-listed CapturedOutput
methods now passing — see the archived doc above), and `core/spring-boot`'s
`org.springframework.boot.SpringApplicationTests` (this one completes but
takes 150s+ for 102 tests, vs. a few seconds for comparable classes —
possibly the same underlying slowdown without fully deadlocking).

**Confirmed pre-existing on unmodified `dev`** (not a regression from the
`capturedoutput` fix): re-ran all 3 hanging classes against a `dev`-tip
binary built *before* any of that session's changes
(`cratonvm-dev-baseline-check.exe`, worktree main `C:\craton\CratonVM` at
`54003fb83`) — identical HANG/slow-FAIL results, byte-for-byte the same
shape. `ConfigurationPropertiesTests`' `err.log` tail matches this doc's
existing symptom exactly: `...DEBUG [org.hibernate.validator.internal.xml
.config.ResourceLoaderHelper] Trying to load META-INF/validation.xml via
Hibernate Validator's class loader` then nothing further — same last line,
same silence.

**New evidence: CPU-sampled busy, not parked** (this doc's own "next steps"
asked for this). Two `Get-Process -Name <exe> | .CPU` samples ~10s apart on
the hung `ConfigurationPropertiesTests` process showed CPU time climbing
(18.0s → 31.0s) — the process is doing real work, not blocked on a lock or
I/O. `CRATONVM_DBG_HANG_SAMPLE=1` (existing sampling probe,
`interpreter.rs:18738`, prints every 200,000th `execute_invoke_kind` call)
shows progress reaches call #200,000 quickly (within seconds) but **never
reaches call #400,000** even after a full 300s window — both with JIT on
and with `--nojit`. The specific method sampled at #200,000 varies between
runs (`org.springframework.core.annotation.TypeMappedAnnotations.scan`,
`java/lang/Thread.getContextClassLoader`, `java/lang/Object.equals`) —
consistent with a large but apparently-nonterminating amount of reflective
work (Spring's meta-annotation scanning / classloader delegation), not one
single fixed infinite loop always hit at the same call. Whether this is a
genuine infinite loop (e.g., a broken cycle-detection guard in annotation
meta-scanning under CratonVM's `Class`/annotation mirror equality) or an
extreme, unbounded slowdown was not determined — the process never produced
a 3rd `HANG_SAMPLE` line even at a 1200s (20-minute) timeout in one run,
which favors "genuinely stuck," but this is not conclusively proven.

Not bisected further this session (out of scope — found while chasing an
unrelated captured-output bug). Whoever picks up this doc's existing "next
steps" (bisect the ~122-commit window, get a real stack dump) should
prioritize it given it now spans at least 5 classes across 3 modules, not
just the original 1.

---

**Status (original, 2026-07-19): OPEN**

## Symptom

`org.springframework.boot.webflux.autoconfigure.actuate.web
.WebFluxManagementChildContextConfigurationIntegrationTests` hangs (no
JUnit summary, process killed on suite-runner timeout). Progress is real
but stops permanently at a consistent point:

```
INFO org.springframework.boot.tomcat.TomcatWebServer -- Tomcat initialized with port 0 (http)
...
INFO [org.apache.coyote.http11.Http11NioProtocol] Initializing ProtocolHandler ["http-nio-auto-1"]
INFO [org.apache.catalina.core.StandardService] Starting service [Tomcat]
INFO [org.apache.catalina.core.StandardEngine] Starting Servlet engine: [Apache Tomcat/11.0.22]
WARN [org.apache.catalina.util.SessionIdGeneratorBase] The default SHA1PRNG algorithm for SecureRandom is not supported by this JVM. Using the platform default.
INFO [org.hibernate.validator.internal.util.Version] HV000001: Hibernate Validator 9.1.0.Final
DEBUG [org.hibernate.validator.messageinterpolation.ResourceBundleMessageInterpolator] Loaded expression factory via original TCCL
DEBUG [org.hibernate.validator.internal.xml.config.ResourceLoaderHelper] Trying to load META-INF/validation.xml via user class loader
DEBUG [org.hibernate.validator.internal.xml.config.ResourceLoaderHelper] Trying to load META-INF/validation.xml via TCCL
DEBUG [org.hibernate.validator.internal.xml.config.ResourceLoaderHelper] Trying to load META-INF/validation.xml via Hibernate Validator's class loader
```

...then nothing further — no exception, no next debug line, no GC/thread
activity resembling progress. Timeout kills the process (300s+, tested up
to 420s in one run) with 0 tests reported.

## Discovery context (not this doc's original bug)

Found while re-verifying
[`../../internal/springboot/spring-boot-webflux-residuals-FIXED.md`](../../internal/springboot/spring-boot-webflux-residuals-FIXED.md)'s
Issue B (a *different*, now-fixed hang in the same class — that one
stalled much earlier, during JUnit discovery, before Tomcat ever started,
and is documented separately as fixed). This is a distinct hang the same
test class now hits *later* in its run, discovered only after merging 122
new `origin/dev` commits into that fix's feature branch.

## Root cause: confirmed NOT related to the webflux-residuals fix; NOT bisected further

**Confirmed via isolated testing** (2026-07-19, this session):

- The webflux-residuals fix (getfield/putfield loader-aware field
  resolution + `MergedAnnotation$Adapt.isIn` identity bridge), tested
  against its own pre-merge `dev` base (`40678d0f5`), does **not** exhibit
  this hang — 3 clean full completions (see the FIXED doc above).
- After merging 122 new `origin/dev` commits into that same branch, this
  NEW hang appeared (different stall point, later in the test).
- **A from-scratch worktree built at pure `origin/dev` tip (commit
  `88fa839cc`), with NONE of the webflux-residuals branch's changes
  present at all**, reproduces the IDENTICAL hang at the IDENTICAL stall
  point (byte-for-byte matching debug-log tail). Confirmed under a
  verified-low-host-load window (39-44GB free RAM, single-digit concurrent
  build processes on this shared box, ruling out resource contention as
  the cause).

This conclusively means: **some commit in the ~122-commit window between
`40678d0f5` and `origin/dev`'s 2026-07-19 tip introduced this hang**,
independent of and unrelated to the webflux-residuals fix. Not bisected to
a specific commit — out of scope for the session that found it. Given the
stall is inside a `ClassLoader` resource-lookup delegation chain
(`ResourceLoaderHelper` walking user classloader → TCCL → Hibernate
Validator's own classloader), the most likely candidates are one of the
classloader-related fixes that landed in that window, e.g.:

- `URLClassLoader.getResourceAsStream` dead-in-real-JDK-mode fix
- `ClassUtils.forName` null-explicit-classloader fix
- The "isolated-loader-*" cluster (`ObjectProvider` generic identity,
  `OnBeanCondition` type deduction, `stop isolated URLClassLoader class
  resolution from silently falling through to the global classpath`)

None of these have been individually tested against this repro; this is a
plausible-candidates list, not a confirmed mechanism.

## Next steps for whoever picks this up

1. Bisect the ~122-commit window (`git log --oneline <40678d0f5>..origin/dev`)
   against this exact repro (single-class, `-Parallel 1`, `-TimeoutSec 300+`,
   verified-low host load) to find the introducing commit.
2. Once found, get a stack/thread dump on timeout (`--stack-dump-on-timeout`
   or equivalent, per this repo's crash-debug tooling) to see exactly which
   native call or lock the `ResourceLoaderHelper` classloader-resource
   lookup is blocked in — the log gives the last debug line before the
   stall, not the blocking frame itself.
3. Check whether other Hibernate-Validator-bootstrapping test classes
   (anything constructing a `jakarta.validation.Validator` under a
   non-trivial classloader hierarchy) hit the same wall — this may be a
   broader-than-webflux regression.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-webflux` | `org.springframework.boot.webflux.autoconfigure.actuate.web.WebFluxManagementChildContextConfigurationIntegrationTests` |
| `core/spring-boot` | `org.springframework.boot.context.properties.ConfigurationPropertiesTests` (full class; found 2026-07-20) |
| `core/spring-boot` | `org.springframework.boot.context.properties.bind.BinderTests` (found 2026-07-20) |
| `module/spring-boot-webmvc` | `org.springframework.boot.webmvc.autoconfigure.WebMvcObservationAutoConfigurationTests` (full class; found 2026-07-20) |
| `core/spring-boot` | `org.springframework.boot.SpringApplicationTests` (completes but 150s+/102 tests — possibly a partial/non-fatal instance of the same slowdown; found 2026-07-20) |
