# Logback `LoggerContext.loggerContextListenerList` (a `final` field) turns null after N successful resets — heap/GC corruption, not a construction bug

**Status: FIXED/RETIRED 2026-07-15**

## Resolution

The original heap/GC diagnosis was incorrect. CratonVM registered
`LoggerContext.<init>()V` as a no-op in three native-registration paths, while
the SLF4J `StaticLoggerBinder` native allocated a real-classed
`LoggerContext` directly instead of invoking its constructor. Consequently the
object had the real 27-slot layout but its final `loggerContextListenerList`
field (slot 17) was never initialized.

The binder now calls `NativeContext::new_object_initialized` for
`LoggerContext`; every no-op constructor registration and every
constructor-bypass shim for `ContextBase` state was removed. Logback now owns
its real context maps, status manager, and turbo-filter list in both native
registration modes.

Final verification used Spring Boot 4.2.0-SNAPSHOT and Logback 1.5.34 with
the unique
`cratonvm-springboot-logback-listenerfield-fullstate-20260715-002.exe` binary:

- `BannerTests`: 6/6 pass with `--nojit`.
- `BannerTests`: 6/6 pass with JIT enabled.
- `SimpleMainTests` and `ConfigurationPropertiesTests` no longer emit the
  `loggerContextListenerList` NPE. Their remaining assertion/configuration
  failures are unrelated residuals outside this retired cluster.

The native-builtins crate also passes `cargo check` after the change.

## Symptom

At least 20 Spring Boot core/module test classes fail with the same
`IllegalStateException`-wrapped `NullPointerException`, always at the same
Logback call site:

```
=> java.lang.IllegalStateException: java.lang.NullPointerException: Cannot invoke "java.util.List.add(Object)" because "this.loggerContextListenerList" is null
   org.springframework.boot.context.logging.LoggingApplicationListener.initializeSystem(LoggingApplicationListener.java:352)
   org.springframework.boot.context.logging.LoggingApplicationListener.initialize(LoggingApplicationListener.java:302)
   org.springframework.boot.context.logging.LoggingApplicationListener.onApplicationEnvironmentPreparedEvent(LoggingApplicationListener.java:248)
   org.springframework.boot.context.logging.LoggingApplicationListener.onApplicationEvent(LoggingApplicationListener.java:225)
   org.springframework.context.event.SimpleApplicationEventMulticaster.doInvokeListener(SimpleApplicationEventMulticaster.java:180)
   ...
   org.springframework.boot.SpringApplication.prepareEnvironment(SpringApplication.java:356)
   org.springframework.boot.SpringApplication.run(SpringApplication.java:316)
   org.springframework.boot.BannerTests.testDefaultBanner(BannerTests.java:69)
 Caused by: java.lang.NullPointerException: Cannot invoke "java.util.List.add(Object)" because "this.loggerContextListenerList" is null
   ch.qos.logback.classic.LoggerContext.addListener(LoggerContext.java:290)
   org.springframework.boot.logging.logback.LogbackLoggingSystem.addLevelChangePropagator(LogbackLoggingSystem.java:329)
   org.springframework.boot.logging.logback.LogbackLoggingSystem.stopAndReset(LogbackLoggingSystem.java:312)
   org.springframework.boot.logging.logback.LogbackLoggingSystem.loadDefaults(LogbackLoggingSystem.java:233)
   org.springframework.boot.logging.AbstractLoggingSystem.initializeWithConventions(AbstractLoggingSystem.java:89)
   org.springframework.boot.logging.AbstractLoggingSystem.initialize(AbstractLoggingSystem.java:65)
   org.springframework.boot.logging.logback.LogbackLoggingSystem.initialize(LogbackLoggingSystem.java:197)
   org.springframework.boot.context.logging.LoggingApplicationListener.initializeSystem(LoggingApplicationListener.java:337)
   [...]
```

(`[...]` above is JUnit's own trailing-frame elision — it is `Caused by`
frame de-duplication, not truncated capture; every log examined has this
same shape all the way down to `LogbackLoggingSystem.initialize`.)

Full logs (each with the complete repeated stack trace) are under
`apps\spring-boot-suite-runner\.suite\results\crashfail-20260714\shard*\logs\`.
Confirmed present, byte-identical stack shape, in at least these logs (this
is a **superset** of the 15 classes originally flagged — the cluster is
substantially larger):

- `shard1/logs/core_spring-boot.org.springframework.boot.BannerTests.out.log` — 6/6 tests fail
- `shard1/logs/...admin.SpringApplicationAdminMXBeanRegistrarTests.out.log` — 4/5 fail
- `shard1/logs/...builder.SpringApplicationBuilderTests.out.log` — 21/23 fail
- `shard1/logs/...context.config.ConfigDataEnvironmentPostP-*.out.log` (x2) — 83/87 and 2/2 fail
- `shard1/logs/...context.event.EventPublishingRunListenerTests.out.log` — 1/2 fail
- `shard1/logs/...context.logging.LoggingApplicationListene-*.out.log` — 3/3 fail
- `shard1/logs/...context.properties.ConfigurationPropertiesTests.out.log` — **only 2/114 fail**
- `shard2/logs/...io.ProtocolResolverApplicationContextInit-*.out.log` — 1/1 fail
- `shard2/logs/...OverrideSourcesTests.out.log` — 2/2 fail
- `shard2/logs/...SimpleMainTests.out.log` — **4/5 fail (1 passes)**
- `shard2/logs/...SpringApplicationAotProcessorTests.out.log` — 6/8 fail
- `shard2/logs/...SpringApplicationExtensionsTests.out.log` — 6/7 fail
- `shard2/logs/...support.AnsiOutputApplicationListenerTests.out.log` — 3/3 fail
- `shard2/logs/...support.EnvironmentPostProcessorApplicati-*.out.log` — **4/16 fail**
- `shard2/logs/...web.servlet.support.SpringBootServletInitializerTests.out.log` — 9/15 fail
- `shard7/logs/module_spring-boot-micrometer-metrics-test...` (x2) — 2/2 fail each
- `shard7/logs/module_spring-boot-restclient-test...` (x2) — 1/1 fail each

The variable partial-pass counts (0/6, 1/5, 2/114, 4/16, 12/16-pass...) are
the key clue — see Analysis.

## Historical analysis (superseded)

### The field is declared `final` — it cannot legally become null after construction

Decompiled the real vendored jar
(`C:\Users\Victor\.gradle\caches\modules-2\files-2.1\ch.qos.logback\logback-classic\1.5.32\...\logback-classic-1.5.32.jar`,
`ch/qos/logback/classic/LoggerContext.class`) with `javap -c -p`:

```
private final java.util.List<ch.qos.logback.classic.spi.LoggerContextListener> loggerContextListenerList;
...
public ch.qos.logback.classic.LoggerContext();
    Code:
       0: aload_0
       1: invokespecial #3    // Method ch/qos/logback/core/ContextBase."<init>":()V
       4: aload_0
       5: iconst_0
       6: putfield      #9    // Field noAppenderWarning:I
       9: aload_0
      10: new           #15   // class java/util/ArrayList
      13: dup
      14: invokespecial #17   // Method java/util/ArrayList."<init>":()V
      17: putfield      #18   // Field loggerContextListenerList:Ljava/util/List;
      20: aload_0
      21: new           #22   // class ch/qos/logback/classic/spi/TurboFilterList
      ...
      75: aload_0
      76: new           #61   // class ch/qos/logback/classic/Logger      (root logger)
      ...
      87: putfield      #68   // Field root:Lch/qos/logback/classic/Logger;
      ...
     136: aload_0
     137: invokevirtual #97   // Method start:()V
     140: return
```

`loggerContextListenerList` is a `private final` field, assigned exactly
once at bytecode offset 17, inside `<init>`, via the standard
`new ArrayList(); dup; invokespecial <init>; putfield` sequence javac emits
for an inline field initializer. Because it is `final`, the JVM spec (and
`javac`) guarantee no other bytecode anywhere in the class — not
`reset()`-style methods, not `stopAndReset()`, nothing — can legally
reassign it after `<init>` returns. **A `final` reference field going from
non-null to null over the lifetime of one object is not achievable through
any legal Java bytecode path; it can only happen through corruption of the
object's heap storage.**

### The partial-pass counts prove the field WAS correctly non-null earlier in the same process

Every class in this cluster runs multiple `SpringApplication.run()` calls in
one CratonVM process (`run-spring-boot-suite.ps1` is one-process-per-*class*,
but each class has 1-114 `@Test` methods executed in the same JVM; each
`SpringApplication.run()` reuses/reset()s the one process-wide SLF4J
`LoggerContext` singleton created via logback-classic's static binder).

If the field initializer were simply skipped or elided during construction,
**every** test in a class would fail identically from the first
`SpringApplication.run()` onward. That is not what the logs show:

- `ConfigurationPropertiesTests`: 112/114 tests **pass** (the shared
  `LoggerContext` is reset correctly 112 times), then the field is null for
  exactly 2.
- `EnvironmentPostProcessorApplicationListenerTests`: 12/16 pass, 4 fail.
- `SimpleMainTests`: 1/5 passes, then the remaining 4 fail with the field
  null.
- `SpringApplicationAdminMXBeanRegistrarTests`: 1/5 passes.

This is the signature of intermittent, timing-dependent heap corruption
(most consistent with a GC-cycle-triggered event) landing on an
already-correctly-initialized object — **not** a deterministic
constructor/field-initializer bug. A construction-time bug would produce
0 passes in every affected class; that is only observed in a subset
(`BannerTests`, `OverrideSourcesTests`, the two `micrometer`/`restclient`
classes, etc.) where the corruption apparently hit on the very first
allocation of that process's `LoggerContext`, which is also consistent with
a probabilistic trigger (some processes get unlucky immediately, others
after dozens of successful cycles).

### Two specific CratonVM JIT mechanisms were checked and ruled out

Given the shape of the bug (`final` field write inside `<init>`, silently
lost), the two most obvious CratonVM JIT-eligibility mechanisms were
inspected against the **real** decompiled bytecode above:

1. **`vm/src/jit/skip_list.rs::classify_init_complexity`** (and
   `should_skip_jit_with_init`, `vm/src/jit/skip_list.rs:281`) — bans JIT
   compilation of any `<init>` containing `putfield`/`putstatic`/
   `monitorenter`/`monitorexit`/`invokedynamic`. `LoggerContext.<init>` has
   a `putfield` at offset 6 (`noAppenderWarning`), so
   `classify_init_complexity` returns `Complex` immediately and this
   constructor is correctly forced to the interpreter. **Ruled out.**

2. **`vm/src/runtime/interpreter.rs::resolve_inline_site`** (added by commit
   `c89c70bf9`, "JIT: constructor inlining … elide proven no-op super-ctor
   calls") — a separate, newer mechanism that inlines `<init>` bodies at
   `new C(args)` call sites and does its own bytecode scan that *does*
   tolerate `putfield`/`getfield` (see `interpreter.rs:30186-30190`). This
   looked like a strong candidate initially. However, its scan also
   **immediately rejects any `new`/`anewarray`/`multianewarray` opcode**
   found anywhere in the callee (`interpreter.rs:30158`,
   `0xbb | 0xbd | 0xc5 => return None`). `LoggerContext.<init>` allocates
   six other objects inline (`ArrayList` at offset 10, `TurboFilterList` at
   21, `ConcurrentHashMap` at 52, `LoggerContextVO` at 64, the root `Logger`
   at 76, another `ArrayList` at 126) — the scan hits the very first `new`
   at offset 10, before it even reaches the `loggerContextListenerList`
   putfield at offset 17, and bails the whole callee out of inlining
   consideration. **Ruled out** — this constructor cannot be selected by
   `resolve_inline_site` either.

Both of CratonVM's own eligibility checks correctly force
`LoggerContext.<init>` to run fully interpreted. That does not clear CratonVM
— it just means the bug is not in either of these two specific,
already-hardened JIT gates. Given the "correctly-initialized, later
corrupted" evidence above, the more likely candidates are:

- A JIT mechanism *not yet audited here* that touches an already-live
  `LoggerContext` object later (e.g. whatever JIT-compiles
  `LogbackLoggingSystem.stopAndReset`/`addLevelChangePropagator` or
  `LoggerContext.addListener` itself, or an inlining/OSR path triggered on
  a *different*, hot call site that happens to alias this object).
- A GC/heap corruption mechanism landing on a `final` object field some
  number of allocations/collections into the run — this matches the shape
  of several already-tracked issues in this codebase (moving-young-gen
  field loss, stale-ref decode after a self-forwarding GC event, and the
  "plain-field slot tearing" family), none of which have been confirmed as
  the specific cause here — this doc should be cross-checked against those
  once a repro is captured with GC tracing enabled.

### Not investigated yet (time-boxed out of this pass)

A live `-Jit off` vs `-Jit on` A/B repro (`run-spring-boot-suite.ps1
-Jit off`) would immediately confirm or rule out JIT involvement entirely
(if the corruption still reproduces with `--nojit`, this is a pure
interpreter/GC bug, not JIT-specific). This was not run: the classpath
files (`<module>\build\cratonvm-test-cp.txt`) are not currently present in
this worktree — reproducing requires `-Setup` first (10-30 min cold Gradle
resolution per `run-spring-boot-suite.md`). Recommended next step for
whoever picks this up.

## Repro

Once `-Setup` has been run for this worktree's `apps\spring-boot` checkout:

```powershell
apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 -SpringBootRoot C:\craton\CratonVM-spring-boot-crashfail-20260714\apps\spring-boot `
  -ClassList <TSV rows for the classes above, e.g.:
    core/spring-boot	org.springframework.boot.context.properties.ConfigurationPropertiesTests
    core/spring-boot	org.springframework.boot.SimpleMainTests
    core/spring-boot	org.springframework.boot.BannerTests> `
  -Start 1 -Count 3 -Parallel 1 -Exe <current dev-tip cratonvm.exe>
```

Because the corruption is intermittent/timing-dependent, prefer
`ConfigurationPropertiesTests` (114 tests, only 2 fail) for a bisect signal
over `BannerTests` (fails from test 1) — a passing-then-failing class gives
a much stronger before/after boundary to instrument around (e.g. wrap
`LoggerContext.addListener`/`stopAndReset` calls with a debug hook that
dumps the object's field slots each cycle).

To test the JIT-involvement hypothesis directly, run the same class list
with `-Jit off` (adds `--nojit`) and compare: if the corruption disappears,
it is JIT-caused (narrows to whichever compiled method is live during that
process's failing cycle); if it persists, it is an interpreter/GC-level bug.

## Related

- Not a match for any existing doc in `docs/known-issues/springboot/` or
  `docs/internal/` — searched for "LoggerContext", "loggerContextListenerList",
  "field initializer", "instance init"; no hits before this doc.
- The `resolve_inline_site` constructor-inlining mechanism (`c89c70bf9`,
  merged via `114de7bde`) was a strong initial suspect given its recency and
  its explicit tolerance of `putfield` inside inlined `<init>` bodies, but is
  concretely ruled out for *this* class by the `new`-opcode ban in its own
  scan (`interpreter.rs:30158`) — worth remembering as a red herring for
  future investigators tracing similar "final field went null" symptoms in
  *simpler* constructors (ones without any inline `new` of their own) where
  that scan would NOT bail early.
- Should be cross-referenced against this codebase's existing GC-field-loss
  family once a repro with tracing is captured (moving-young-gen field loss,
  stale-ref decode, plain-field-slot tearing, self-forwarding UAF) — the
  "final field silently nulled after N successful uses of the same object"
  shape matches that family's signature better than any JIT-eligibility gap
  found so far.
