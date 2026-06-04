# Bug: JUnit5 jupiter discovery finds 0 tests on CratonVM (HIGH VALUE)

Fixing this unblocks running **every JUnit5 suite** on CratonVM (Apache Commons Math,
and any modern `@Test` suite). Independent of the other `continue_prompt_*` bugs.

## State (corrected)
With a CORRECT *absolute* classpath, CratonVM runs the JUnit5 console launcher to **rc=0
with no crashes** — BufferedWriter, BreakIterator, and the `ParameterProvider$2.add`
NoSuchMethodError are all gone (earlier "crash/blocked" reports were a relative-classpath /
wrong-cwd repro artifact). ServiceLoader finds all 3 engines; `Method.getAnnotations()`
returns `[Test]`. All match HotSpot.

## The gap
A programmatic launch that bypasses picocli entirely —
`LauncherFactory.create().execute(request(selectClass("org.apache.commons.math4.transform.TransformUtilsTest")))`
— finds **FOUND=0 on CratonVM vs 4 on HotSpot**, *silently* (rc=0, no warning/exception).
The jupiter engine's `isTestMethod`/`isTestClass` predicate returns false on CratonVM.

Ruled out as causes (verified equal to HotSpot): ServiceLoader (3 engines),
`Method.getAnnotations()`, `Class.getModifiers()`.

## Confound to avoid
An ad-hoc *flat* classpath can pull TWO `org.junit.jupiter.api.Test` classes (the
console-standalone jar bundles jupiter-api; a separate `junit-jupiter-api` jar adds
another), so `getAnnotation(Test.class)` / `AnnotationSupport.findAnnotatedMethods(Test.class)`
read 0 on BOTH VMs — a probe artifact, not a CratonVM signal.

## Next steps
1. Build a **single-jupiter-api** classpath (exactly one `Test.class` on the path) so the
   reflection probes become meaningful.
2. Trace jupiter's discovery on CratonVM — instrument / step through
   `org.junit.platform.commons.util.ReflectionUtils.findMethods(testClass, predicate, TOP_DOWN)`
   and `AnnotationUtils.findAnnotation(method, Test.class)`. Suspects:
   - method enumeration **order/dedup** in `ReflectionUtils.findMethods` (merges declared +
     inherited, de-dups by signature),
   - annotation **type-identity** comparison (`annotationType() == Test.class`),
   - the **meta-annotation** walk (`@Test` is meta-annotated `@Testable`).
3. Probes (programmatic, bypass picocli): `JUnitProbe` (LauncherFactory FOUND/SUCCEEDED),
   `EngineProbe` (`ServiceLoader<TestEngine>`), `AnnProbe3` (modifiers +
   `AnnotationSupport.findAnnotatedMethods`). Compile against the standalone jar; run
   `-cp "bench;<abs cp>"`.

## Repro essentials
- VM: `target/release/cratonvm.exe --java-home "C:/Program Files/Java/jdk-25"`.
  HotSpot ref: `C:/Program Files/Java/jdk-25/bin/java.exe`.
- Standalone jar: `.bench-cache/junit-platform-console-standalone-1.10.2.jar`.
- Absolute test cp (build it; do NOT use the relative `cm_transform_cp.txt`):
  standalone jar `;` `apps/_test-suites/commons-math/commons-math-{transform,core}/target/{classes,test-classes}`
  `;` commons-numbers-{core,complex,arrays,angle}-1.3 + commons-rng-{simple,core,client-api}-1.7 +
  commons-math3-3.6.1 jars from `~/.m2`.
- Memory: `reference_junit5_console_launcher`.
