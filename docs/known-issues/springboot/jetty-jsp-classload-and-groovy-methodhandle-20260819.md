# Two CratonVM-only Spring Boot defects found triaging a 3-GC full-suite run (2026-08-19)

**Status: OPEN, each confirmed against HotSpot on the same fixed classpath/cwd,
neither root-caused to a specific CratonVM source location, no fix attempted.**

Found while checking whether a Spring Boot 3-GC (Generational/G1/ZGC) sweep's
93/90/91 FAIL counts (up from a 39-FAIL baseline on 2026-08-05) represented a
real regression — see
`apps/spring-boot-suite-runner/RESULTS-20260819-3gc-azure-CORRECTION.md` for
the full accounting (most of the apparent increase was a harness cwd bug and
a missing `--add-opens`, not CratonVM). These two are what survived that
correction as genuine, individually HotSpot-verified divergences.

## 1. Jetty's embedded JSP servlet fails to class-load

```
Caused by: jakarta.servlet.UnavailableException: Class loading error for
  holder jsp==org.apache.jasper.servlet.JspServlet@19c47{jsp=null,order=3,
  inst=false,async=true,src=EMBEDDED:<null>,STARTING}
```

**Reproduction:**
```bash
cd /data/cratonvm/apps/spring-boot/module/spring-boot-jetty
CP="/data/cratonvm/apps/spring-boot/sb-runner:$(cat build/cratonvm-test-cp.txt)"
# CratonVM: fails with the UnavailableException above
<cratonvm> --java-home <jdk25> --Xmx 2g -cp "$CP" SbRunner \
  org.springframework.boot.jetty.autoconfigure.AutoConfigureWebServerJettyServletTests
# HotSpot: SBRUNNER_RESULT tests=1 failed=0 ... — passes clean
<jdk25>/bin/java -cp "$CP" SbRunner \
  org.springframework.boot.jetty.autoconfigure.AutoConfigureWebServerJettyServletTests
```

Likely affects the rest of the `module/spring-boot-jetty` cluster that shares
this symptom in the same suite run (`AutoConfigureWebServerJettyServletTests`,
`JettyMetricsAutoConfigurationTests`,
`JettyServletWebServerAutoConfigurationTests`,
`JettyServletWebServerFactoryTests`,
`JettyServletWebServerMvcIntegrationTests`,
`JettyServletWebServerServletContextListenerTests`,
`JettyWebServerFactoryCustomizerTests`) — only the first was individually
confirmed against HotSpot; the rest are inferred from sharing the same suite
run's FAIL list and the `jakarta.servlet.UnavailableException`/JSP-loading
theme, not independently re-verified.

**Likely related to (not confirmed as the same bug):** a Jetty/Jasper
`ClassNotFoundException: org.apache.jasper.servlet.JspServlet` cluster
investigated earlier this session while triaging the Commons Math suite. That
investigation traced the symptom to Jetty's
`WebAppClassLoader$Context.isHiddenClass()` reading a corrupted pattern set
from `ClassMatcher.getPatterns()` — `AbstractCollection.toArray(T[])` on a
`Map`-keySet-backed custom `Set` (which is what Jetty's `ClassMatcher` is)
returning a null-content array, making every class — including one Jetty had
just successfully resolved — look "hidden," so the loader discarded the
correct answer and reported the class unfindable. That investigation's own
write-up could not be located in the current docs tree while filing this page
(likely retired or pruned since) so the mechanism is not re-verified here.
**Check first** whether
`fixed-bugs/jit-multianewarray-allocated-every-level-with-classid-0-FIXED-20260817.md`
or
`fixed-bugs/interpreter-checkcast-and-instanceof-re-resolved-their-target-every-time-FIXED-20260818.md`
already covers this `toArray`/array-allocation path — if so, this page may
just need a re-verify on current `dev` rather than a fresh investigation from
scratch.

## 2. Groovy closure → `MethodHandle` adaptation throws `WrongMethodTypeException`

```
Caused by: java.lang.invoke.WrongMethodTypeException: cannot convert
  MethodHandle(beans,Closure)Object to (Object[])Object
```

**Reproduction:**
```bash
cd /data/cratonvm/apps/spring-boot/core/spring-boot
CP="/data/cratonvm/apps/spring-boot/sb-runner:$(cat build/cratonvm-test-cp.txt)"
# CratonVM: fails with the WrongMethodTypeException above
<cratonvm> --java-home <jdk25> --Xmx 2g -cp "$CP" SbRunner \
  org.springframework.boot.BeanDefinitionLoaderTests
# HotSpot: SBRUNNER_RESULT tests=13 failed=0 ... — passes 13/13 clean
<jdk25>/bin/java -cp "$CP" SbRunner org.springframework.boot.BeanDefinitionLoaderTests
```

A `MethodHandle` built for a 2-arg shape `(beans, Closure) -> Object` is being
invoked as if it had a varargs/array shape `(Object[]) -> Object` — an
arity/signature mismatch in whatever CratonVM code adapts a Groovy closure's
call site to a `MethodHandle`. Only `BeanDefinitionLoaderTests` was
individually confirmed against HotSpot; the same suite run's FAIL list has 8
more classes sharing the exact `WrongMethodTypeException` text, all
Groovy-configuration-adjacent, consistent with (but not confirmed as) one
shared root cause: `SpringBootTestGroovyConfigurationTests`,
`SpringBootTestGroovyConventionConfigurationTests`,
`SpringBootTestMixedConfigurationTests`,
`module/spring-boot-cache/CacheAutoConfigurationTests`,
`module/spring-boot-groovy-templates/GroovyTemplateAutoConfigurationTests`,
`module/spring-boot-hazelcast/HazelcastAutoConfiguration{Client,Server}Tests`,
`module/spring-boot-thymeleaf/Thymeleaf{Reactive,Servlet}AutoConfigurationTests`.

## What would confirm/fix either

Not attempted here — this page is a triage handoff, not a root-cause. For (1),
start by checking the two FIXED docs named above against a fresh
`ClassMatcher`/`toArray`-shaped repro (the earlier session's `MapBackedSetProbe`
minimal-repro pattern, if still findable, would settle it in minutes). For
(2), a minimal Groovy-closure-to-`MethodHandle` repro isolating just the
`invokedynamic` call site `BeanDefinitionLoader` uses would separate "one
JIT/interpreter bug in method-handle adaptation" from "nine independent Groovy
integration issues."

## Related

* `apps/spring-boot-suite-runner/RESULTS-20260819-3gc-azure-CORRECTION.md` —
  the full accounting this page's two defects were extracted from, including
  the two harness confounds (cwd, missing `--add-opens`) that explain most of
  the raw FAIL-count increase this run appeared to show.
* `apps/spring-boot-suite-runner/RESULTS-20260819-3gc-azure.md` — the raw
  3-GC sweep results these came from.
