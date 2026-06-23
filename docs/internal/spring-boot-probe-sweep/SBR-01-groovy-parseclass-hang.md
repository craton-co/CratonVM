# SBR-01 — `GroovyClassLoader.parseClass` hangs (no output, >60 s)

**Status:** 🔴 Open — **HANG** (CratonVM-only; HotSpot ~1 s)
**Recommendation:** **HANDOFF** — deep (Groovy compiler under CratonVM); dominant Spring Boot blocker (`groovyscripts`, build conventions).
**Binary:** `cvsbfull.exe` (dev `df11ac00`) vs HotSpot `jdk-25`.

## Affected probes (9 — one root cause)

`GroovyParseProbe`, `GroovyDeepProbe`, `GroovyMultiProbe`, `GroovyNestProbe`,
`GroovyNpeProbe`, `GroovyNpeDeep`, `GroovyScriptProbe`, `GroovyScaleProbe`,
`GroovyThreadProbe`. Every probe that drives `groovy.lang.GroovyClassLoader`
hangs; every non-Groovy probe in the same sweep completed.

## Symptom

`GroovyParseProbe` compiles a trivial class via `GroovyClassLoader.parseClass`:

```java
GroovyClassLoader gcl = new GroovyClassLoader();
Class<?> c = gcl.parseClass("class Foo {\n int bar(int x){ int y=x+1; return y*2 }\n}\n", "Foo.groovy");
System.out.println("PARSED OK: " + c.getName());
```

- **HotSpot:** prints `PARSED OK: Foo`, exits 0 in ~1 s.
- **CratonVM:** prints **nothing** and never returns; killed at the 60 s timeout
  (`cv/GroovyParseProbe.log` is empty apart from the VM banner). So the hang is
  inside `GroovyClassLoader` construction / `parseClass` compilation, before any
  user output.

## Root cause (hypothesis)

Hang is in Groovy's compile pipeline (antlr4 parse → ASM class generation →
`defineClass`) running under CratonVM. Candidates, in order of suspicion:
1. An infinite/again-and-again loop in a CratonVM native reached during Groovy
   class generation (cf. the documented *synthetic-stub upgrade rescan storm*,
   where a classpath rescan recurs per allocation on a large classpath — Groovy
   loads many classes during first compile).
2. A monitor/`wait` that never wakes (Groovy uses locks around its class cache).
3. Catastrophic throughput (like SBR-02) — but 60 s for a one-method class would
   be ~1000× slower than HS, which points to a true loop, not mere slowness.

A stack/thread dump at timeout is needed to localize (re-run **without**
`--stack-dump-on-timeout 0`).

## Repro

```bash
cd C:/craton/CratonVM/apps/spring-boot/buildSrc
CP="runner;$(cat test-classpath.txt)"
# hangs (kill after ~60s):
"C:/craton/CratonVM-sbfull/target/release/cvsbfull.exe" --java-home "C:/Program Files/Java/jdk-25" -cp "$CP" GroovyParseProbe
# passes in ~1s:
"C:/Program Files/Java/jdk-25/bin/java.exe" -cp "$CP" GroovyParseProbe
```

To get a stack: drop `--stack-dump-on-timeout 0` and add a watchdog dump, or run
under `CRATONVM_DBG_*` tracing for the compile path.

## Impact

Blocks all Spring Boot build logic that evaluates Groovy at build time
(`SpringRepositoriesExtensionTests` and the `groovyscripts` package — a known
buildSrc hang). High value to fix; large surface to investigate → handoff.
