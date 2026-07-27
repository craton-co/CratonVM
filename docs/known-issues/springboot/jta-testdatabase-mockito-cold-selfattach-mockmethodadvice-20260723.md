# `JtaAutoConfigurationTests` / `TestDatabaseAutoConfigurationNoEmbeddedTests`: Mockito `NoClassDefFoundError: MockMethodAdvice` on first mock use in a fresh forked JVM — cross-references an already-characterized, non-Spring-specific CratonVM gap

**Status: OPEN — found 2026-07-23. Not a new bug; cross-filed against an already-tracked mechanism, no fix landed for these two classes**

## Symptom

| Module | Class | Failures |
|---|---|---:|
| `module/spring-boot-transaction` | `org.springframework.boot.transaction.jta.autoconfigure.JtaAutoConfigurationTests` | 6/6 |
| `module/spring-boot-jdbc-test` | `org.springframework.boot.jdbc.test.autoconfigure.TestDatabaseAutoConfigurationNoEmbeddedTests` | 1/2 |

```
java.lang.IllegalStateException: Could not initialize plugin: interface org.mockito.plugins.MockMaker (alternate: null)
	at org.mockito.internal.configuration.plugins.PluginLoader$1.invoke(PluginLoader.java:85)
	...
 Caused by: java.lang.IllegalStateException: Internal problem occurred, please report it. Mockito is unable to load
 the default implementation of class that is a part of Mockito distribution. Failed to load interface
 org.mockito.plugins.MockMaker
	at org.mockito.internal.configuration.plugins.DefaultMockitoPlugins.create(DefaultMockitoPlugins.java:105)
	...
 Caused by: java.lang.reflect.InvocationTargetException: java.lang.NoClassDefFoundError: org.mockito.internal.creation.bytebuddy.MockMethodAdvice
 Caused by: java.lang.NoClassDefFoundError: org.mockito.internal.creation.bytebuddy.MockMethodAdvice
```

Both classes' `.err.log` shows the standard "Mockito is currently
self-attaching..." warning immediately followed by `[cratonvm]
System.exit(1) called` with no visible Java stack trace in `err.log` — the
actual `NoClassDefFoundError` only surfaces in `.out.log`, wrapped inside
whatever Spring bean-creation exception first triggered `Mockito.mock(...)`.

Full logs:
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260723/shard6/logs/module_spring-boot-transaction.org.springframework.boot.transaction.jta.autoconfigure.JtaAutoC-f46d2251436d.out.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260723/shard3/logs/module_spring-boot-jdbc-test.org.springframework.boot.jdbc.test.autoconfigure.TestDatabaseAuto-b797bd517673.out.log`

## Root cause — already characterized elsewhere, not re-derived here

This exact `MockMaker`-init → `MockMethodAdvice` `NoClassDefFoundError` shape
is the same family already deep-dived in
`docs/internal/fixed-suite-bugs/kafka-suite-bugs/bug-09-mockito-inline-mockmaker-selfattach.md`
(present in this worktree) and referenced as still-partially-open in
`docs/internal/fixed-suite-bugs/spring/CRATONVM-SPRING-GENUINE-BUGLIST.md`
(`origin/dev`, not yet merged into this worktree as of its HEAD): Mockito's
inline mock-maker needs a `java.lang.instrument.Instrumentation` via
self-attach, and CratonVM's self-attach support has a **cold-start gap** —
the bulk of the self-attach + `MockMethodAdvice` machinery was fixed
2026-06-12 (per bug-09's "Progress" section), but the genuine-buglist doc
documents a **still-reproducing** "first fork in a brand-new
process/`ClassLoader` chain sometimes fails cold, later forks in the same
process succeed" pattern via the standalone `BBProbe4.java` repro. Both
`JtaAutoConfigurationTests` and `TestDatabaseAutoConfigurationNoEmbeddedTests`
are single-class-per-forked-JVM runs under this suite runner (a fresh
`cratonvm` process per class), so every Mockito use in these classes is a
guaranteed "cold" first-use — consistent with the cold-attach hypothesis, not
independently re-confirmed here (no time-budget for a live self-attach trace
in this session).

**Not confirmed:** whether this is literally the same code path as the
`BBProbe4` repro, or a distinct-but-similarly-shaped cold-init gap specific
to how these two Spring Boot tests reach `Mockito.mock()` (both go through
`@Bean` factory-method bodies rather than `@Mock` field injection — untested
variable). Flagged here as the most likely shared mechanism given the
identical `Caused by` chain, not as a verified match.

## Distinguish from siblings already fixed by other work

Several sibling classes in this same 2026-07-23 rerun batch show the
identical `MockMethodAdvice`/`System.exit(1)`-with-no-trace signature:
`org.springframework.boot.devtools.autoconfigure.DevToolsEmbeddedDataSourceAutoConfigurationTests`
and
`org.springframework.boot.jetty.autoconfigure.servlet.JettyServletWebServerServletContextListenerTests`.
Both of those are **already fixed on `origin/dev`** by later work not yet
merged into this worktree (`fix/devtools-mockadvice-20260725`'s RwLock
self-deadlock + `WeakKey`/`LatentKey` `equals()` bridge fix, and
`fix/forkedclasspath-parent-delegation-20260726`'s cross-package
`Method.invoke` fix, respectively) — see the "still-open vs. already-fixed
upstream" breakdown in the session summary. Whether either of those fixes
also happens to close `JtaAutoConfigurationTests`/
`TestDatabaseAutoConfigurationNoEmbeddedTests` was **not verified** (no
rebuild performed this session, out of scope) — worth a fast re-check before
any fresh investigation here.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-transaction` | `org.springframework.boot.transaction.jta.autoconfigure.JtaAutoConfigurationTests` |
| `module/spring-boot-jdbc-test` | `org.springframework.boot.jdbc.test.autoconfigure.TestDatabaseAutoConfigurationNoEmbeddedTests` (1 of 2 methods; the other, `applyReplace`, passes) |
