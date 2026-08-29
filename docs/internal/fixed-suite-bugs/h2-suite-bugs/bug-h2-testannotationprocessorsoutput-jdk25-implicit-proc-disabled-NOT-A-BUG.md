# `org.h2.test.db.TestFunctions#testAnnotationProcessorsOutput` fails on JDK25 — NOT a CratonVM bug (upstream H2/JDK25 incompatibility)

## Status
**NOT A CRATONVM BUG — closed, no fix made.** Investigated and root-caused
2026-07-22 on branch `fix/h2-annotproc-20260722` (no source changes; this
doc merged to `dev` to record the finding). Reproduces **identically** on
HotSpot JDK25 and CratonVM — this is an upstream incompatibility between an
old H2 test and a JDK 25 `javac` behavior change, not a VM defect.

## Background
This investigation was spawned as a residual of
[bug-h2-formatter-datetime-conversion-unimplemented.md](bug-h2-formatter-datetime-conversion-unimplemented.md):
after that `%t`/`%T` `Formatter` fix landed, `org.h2.test.db.TestFunctions`'s
`test()` method ran further than before and stopped at
`testAnnotationProcessorsOutput()` (line 1894) instead. The premise handed
into this investigation was that "HotSpot passes this specific sub-test, per
prior investigation" — that premise does not hold up under direct testing in
this environment (see Verification below) and the real story is more
interesting: it doesn't reliably reach this sub-test *at all* through the
normal `test()` entry point on either VM, because of an unrelated, earlier,
already-known environment/classpath issue
(`testFileRead`, `AssertionError: Expected: true got: false`, noted as
"different, unrelated" in the formatter-fix doc above). Isolating
`testAnnotationProcessorsOutput()` directly via reflection (bypassing
`test()`) is what let this be checked head-to-head.

## Severity
**N/A** — not a bug. Filed under `` per investigation-tracking
convention (mirrors `docs/internal/*-NOT-A-BUG.md` closures elsewhere in this
repo) so the residual doesn't get re-investigated from scratch later.

## Affected test class
`org.h2.test.db.TestFunctions` (`testAnnotationProcessorsOutput`) — fails
identically on **both** HotSpot JDK25 and CratonVM.

## Symptom
```
java.lang.AssertionError: Failure
	at org.h2.test.TestBase.fail(TestBase.java:334)
	at org.h2.test.TestBase.fail(TestBase.java:308)
	at org.h2.test.db.TestFunctions.testAnnotationProcessorsOutput(TestFunctions.java:1898)
```
Line 1898 is the unconditional `fail();` immediately after
`callCompiledFunction(...)` at TestFunctions.java:1894-1905:
```java
private void testAnnotationProcessorsOutput() {
    try {
        System.setProperty(TestAnnotationProcessor.MESSAGES_KEY, "WARNING,foo1|ERROR,foo2");
        callCompiledFunction("test_annotation_processor_warn_and_error");
        fail();                                          // <-- reached: no exception was thrown
    } catch (SQLException e) {
        assertEquals(ErrorCode.SYNTAX_ERROR_1, e.getErrorCode());
        assertContains(e.getMessage(), "foo1");
        assertContains(e.getMessage(), "foo2");
    } finally {
        System.clearProperty(TestAnnotationProcessor.MESSAGES_KEY);
    }
}
```
The test expects `callCompiledFunction` to throw a `SQLException` (because
`org.h2.test.ap.TestAnnotationProcessor` — registered via
`../../../../apps/META-INF/services/javax.annotation.processing.Processor` — is supposed to
emit a `WARNING`/`ERROR` diagnostic pair during compilation of the dynamic
`CREATE ALIAS ... AS $$ ... $$` SQL function, which H2's
`SourceCompiler.handleSyntaxError` then converts into
`ErrorCode.SYNTAX_ERROR_1`). Instead, `callCompiledFunction` returns
normally: the alias compiles and runs with no error or warning, so `fail()`
is reached unconditionally.

## Root cause
**JDK 25's `javac` no longer performs implicit annotation-processor
discovery via the classpath by default.** `SourceCompiler.javaxToolsJavac`
(`src/main/org/h2/util/SourceCompiler.java`) calls:
```java
JAVA_COMPILER.getTask(writer, fileManager, null, null, null, compilationUnits).call();
```
with `options = null` — i.e. no `-processor`/`-proc:full`/`-proc:only`
flag. On older JDKs, `javac` would still discover `TestAnnotationProcessor`
via `ServiceLoader` against `../../../../apps/META-INF/services/javax.annotation.processing.Processor`
on the compile classpath and run it. On this environment's JDK
(Temurin 25.0.3+9 LTS), that implicit discovery path is a no-op: the
processor is never instantiated, `getSupportedAnnotationTypes()` (where
`TestAnnotationProcessor` emits its test messages) is never called, and
compilation simply succeeds with an empty diagnostic stream. This is a
`javac` behavior/default change upstream in the JDK, not anything
CratonVM-specific — CratonVM is running the *real* JDK25 `javac` classes as
ordinary bytecode in this configuration (`--java-home /home/victor/jdk25`),
so it inherits this exact behavior by construction.

Confirmed with a minimal, H2-independent repro (no CratonVM, no H2 code at
all) that isolates just the `javax.tools.JavaCompiler` call H2 makes:
```java
JavaCompiler jc = ToolProvider.getSystemJavaCompiler();
StandardJavaFileManager fm = jc.getStandardFileManager(null, null, null);
// ... one throwaway compilation unit ...
boolean ok = jc.getTask(writer, fm, null, /* options */ null, null, units).call();
```
- Without any `-proc` option (H2's actual call shape): `ok=true`,
  `output=""` — the processor (present on the classpath, with a correct
  service file) is silently never invoked, on **both** HotSpot and
  CratonVM.
- With `options = List.of("-proc:full")` added: `ok=false`,
  `output="warning: foo1\nerror: foo2\n1 error\n1 warning\n"` — the
  processor fires exactly as `TestAnnotationProcessor`/the H2 test expects,
  confirming the processor, its service registration, and its message
  logic are all fine; the only thing missing is the explicit `-proc` opt-in
  that JDK25 now requires and that H2's `SourceCompiler` never passes.

This makes `testAnnotationProcessorsOutput` (and, more broadly, any code
path through `SourceCompiler.javaxToolsJavac` that depends on H2's other
bundled annotation processors, if any exist) an **upstream H2 test/JDK25
incompatibility**, unrelated to and unfixable within CratonVM — the fix, if
one is wanted, belongs in H2's `SourceCompiler.java` (pass `-proc:full`
explicitly), not here.

## Verification (2026-07-22)
Both runs below use `../../../../apps/h2database/h2` in the (pre-existing)
`/data/wt-h2-fail-triage-20260721` worktree, target/classes +
target/test-classes + `craton-testcp.txt` on the classpath, JDK
`/home/victor/jdk25` (Temurin 25.0.3+9 LTS).

**Running `TestFunctions` end-to-end fails before ever reaching this
sub-test, on both VMs**, due to the separate, pre-existing `testFileRead`
issue (`AssertionError: Expected: true got: false` at
`TestFunctions.java:638`, `assertTrue(fileSize > 0)` on a JAR classpath
entry) — reproduced directly on HotSpot in this environment, refuting the
"HotSpot passes" premise this investigation was handed. `testFileRead` runs
(line 132) before `testToCharFromDateTime` (line 135) and
`testAnnotationProcessorsOutput` (line 143) in `test()`'s call sequence, so
neither can be reached through the normal `test()`/`testFromMain()` entry
point in this classpath configuration on either VM.

**Isolating `testAnnotationProcessorsOutput()` directly** (a small
reflection-based driver: `new TestFunctions()`, `.init()`, then
`getDeclaredMethod("testAnnotationProcessorsOutput").setAccessible(true).invoke(t)`,
bypassing `test()`/`testFileRead` entirely) gives a clean, apples-to-apples
comparison:

- **HotSpot JDK25**: `AssertionError: Failure` at
  `TestFunctions.java:1898` — **fails**, same stack trace shape as
  CratonVM.
- **CratonVM** (`cratonvm-h2-annotproc-20260722 --java-home
  /home/victor/jdk25`): `AssertionError: Failure` at
  `TestFunctions.java:1898` — **fails identically**.
- Manually adding the missing
  `../../../../apps/META-INF/services/javax.annotation.processing.Processor` file to
  `target/test-classes` (it is absent from this Maven build's test-classes
  output — H2's `pom.xml` `<testResources>` only copies specific
  `.properties`/`.sql` files from `src/test`, not `../../../../apps/META-INF/**`) did **not**
  change the outcome on either VM, ruling out "missing service file" as the
  (sole) cause and pointing at the `javac`-level implicit-processing default
  instead — confirmed directly per the minimal repro above.

## Recommendation
No CratonVM change needed. If this test's `FAIL` status is worth clearing
in the suite tally, the actual fix belongs upstream in H2's
`SourceCompiler.javaxToolsJavac` (add `"-proc:full"` to the `getTask(...)`
options), which is out of scope for a CratonVM bug fix — H2 is unmodified
third-party test-suite source in this repo. Not pursued further here.

## Repro
```bash
# End-to-end (fails earlier, at testFileRead, on both VMs in this env):
cd apps/h2database/h2
<cratonvm-bin> --java-home /home/victor/jdk25 \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.db.TestFunctions

# Isolated (reflection-based driver calling testAnnotationProcessorsOutput()
# directly) — fails identically on HotSpot and CratonVM:
<cratonvm-bin-or-real-java> -cp "target/classes:target/test-classes:$(cat craton-testcp.txt):<driver-dir>" AnnotProcRepro

# Minimal, H2-independent confirmation of the javac default-behavior change:
<cratonvm-bin-or-real-java> -cp "target/test-classes:<driver-dir>" ApCheck
# ApCheck calls: ToolProvider.getSystemJavaCompiler().getTask(w, fm, null, null, null, units).call()
# with org.h2.test.ap.TestAnnotationProcessor + its service file on the classpath.
# Add options = List.of("-proc:full") to see the processor actually fire.
```
