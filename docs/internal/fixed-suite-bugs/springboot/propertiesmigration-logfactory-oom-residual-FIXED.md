# `PropertiesMigrationListenerTests.sampleReport` OOMs inside `LogFactoryImpl`/`Hashtable.rehash` after the commons-logging fix

**Status: OPEN — found 2026-07-18**

## Context

Residual of [`conditionevaluationreport-capturedoutput-empty-cluster.md`](-capturedoutput-empty-cluster-FIXED.md)
(now FIXED/archived). That fix removed a native override that made
`org.apache.commons.logging.LogFactory.getLog(...)` always return a fake,
silently-swallowing `Log` — real commons-logging bytecode now runs and
does its own SLF4J-bridge discovery. This is a **newly-exposed** residual:
the fake `Log` previously short-circuited every commons-logging call site
before it could reach whatever underlying bug this is, so this failure
mode did not previously manifest under CratonVM at all.

## Symptom

```
JUnit Jupiter:PropertiesMigrationListenerTests:sampleReport(CapturedOutput)
    => java.lang.IllegalStateException: java.lang.OutOfMemoryError: Java heap space (alloc_array length 142606335)
       org.springframework.boot.SpringApplication.handleRunFailure(SpringApplication.java:827)
       org.springframework.boot.SpringApplication.run(SpringApplication.java:331)
       org.springframework.boot.context.properties.migrator.PropertiesMigrationListenerTests.sampleReport(PropertiesMigrationListenerTests.java:51)
     Caused by: java.lang.OutOfMemoryError: Java heap space (alloc_array length 142606335)
       java.util.Hashtable.rehash(Hashtable.java:418)
       java.util.Hashtable.addEntry(Hashtable.java:440)
       java.util.Hashtable.computeIfAbsent(Hashtable.java:1042)
       org.apache.commons.logging.impl.LogFactoryImpl.getInstance(LogFactoryImpl.java:782)
       org.apache.commons.logging.impl.LogFactoryImpl.getInstance(LogFactoryImpl.java:760)
       org.apache.commons.logging.LogFactory.getLog(LogFactory.java:921)
       org.springframework.core.env.AbstractPropertyResolver.<init>(AbstractPropertyResolver.java:86)
       org.springframework.boot.context.properties.source.ConfigurationPropertySourcesPropertyResolver.<init>(ConfigurationPropertySourcesPropertyResolver.java:40)
```

`142606335` implies `LogFactoryImpl`'s per-instance `Hashtable instances`
field grew through roughly 24 doubling rehashes (`(cap << 1) + 1` from the
default initial capacity of 11) before OOMing — i.e. the table held **tens
of millions of distinct entries**, not a handful of duplicate/near-duplicate
keys.

## Root cause (hypothesis — not confirmed; two mechanisms ruled OUT this session)

On this module's exact classpath, commons-logging 1.3.6's discovery picks
the classic `org.apache.commons.logging.impl.LogFactoryImpl` (JUL-backed
`Jdk14Logger`), not the newer `Slf4jLogFactory` auto-detected on other
modules' classpaths (confirmed via a standalone repro dumping
`LogFactory.getFactory().getClass()` — worth investigating separately why
discovery differs per-module, though that alone isn't the OOM's cause).

Two candidate mechanisms were checked and **ruled out** with standalone
repros against this same classpath + binary
(`cratonvm-captured-output-fix-20260717.exe`):

1. **`Hashtable.computeIfAbsent`/`rehash` correctness** — a loop inserting
   5000 genuinely distinct keys into a fresh `Hashtable` (mirroring
   `LogFactoryImpl.instances`'s growth pattern) completed cleanly through
   several rehash cycles, ending at the expected `size()==5000`. Not a
   `Hashtable` capacity/rehash bug in general.
2. **`Class.getName()`/`String` identity or hashCode instability** — calling
   `SomeClass.class.getName()` repeatedly returns the same (`==`) interned
   `String` with a stable `hashCode()`; calling
   `LogFactory.getLog(SomeClass.class)` 2000 times in a loop (same class
   every time) correctly keeps `instances.size()` at a small constant — not
   a "identical key treated as new each time" bug.

Since `AbstractPropertyResolver.<init>` always requests the log for the
*same* class (`AbstractPropertyResolver.class`), and that repeated-identical-
key case is confirmed to correctly dedupe, the `Hashtable` growing into the
tens-of-millions strongly suggests `LogFactory.getLog` is being called with
**genuinely distinct keys** an enormous number of times — most plausibly a
runaway/infinite recursion or proxy-class-generation loop somewhere in
`ConfigurationPropertySourcesPropertyResolver`'s (or a caller's)
construction path that is unrelated to logging itself and was simply never
reached before (the fake `Log` from the now-fixed doc always
short-circuited before `LogFactoryImpl` ever ran). Not root-caused at the
source level this session — would need a live repro with an instrumented
`LogFactoryImpl.getInstance` (dump the requested name on every call) to see
whether the keys are genuinely unique and, if so, trace what's minting them.

## Affected classes

| Module | Class |
|---|---|
| `core/spring-boot-properties-migrator` | `org.springframework.boot.context.properties.migrator.PropertiesMigrationListenerTests` (1 of 1 test, `sampleReport`) |
