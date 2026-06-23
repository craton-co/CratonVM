# H2 — in-process javac "compiler message file broken" (CREATE ALIAS/TRIGGER)

## Status
**OPEN** — jdk.compiler module resource loading.

## Severity
**MEDIUM** — fails H2 tests that compile Java source for user functions/triggers.

## Affected test classes (mem config)
TestView, TestCases, TestTriggersConstraints. (TestFunctions also exercises it
but fails on HotSpot too in this harness → excluded.)

## Symptom
```
org.h2.jdbc.JdbcSQLSyntaxErrorException: Syntax error in SQL statement
"compiler message file broken: key=javac.msg.resource ..."
```
H2's `org.h2.util.SourceCompiler` calls
`ToolProvider.getSystemJavaCompiler()` and runs an in-process `CompilationTask`
to compile the Java body of `CREATE ALIAS ... AS '<java source>'` /
`CREATE TRIGGER ... AS '<java source>'`. The compiler runs far enough to emit a
diagnostic, but formatting it fails: `"compiler message file broken: key=..."`
is javac's fallback when its message `ResourceBundle` cannot be loaded.

## HotSpot behavior
PASS — the `jdk.compiler` module's resources load normally.

## Root cause (hypothesis)
The in-process compiler (`com.sun.tools.javac.*`, module `jdk.compiler`) cannot
load its resource bundles
(`com/sun/tools/javac/resources/compiler.properties`,
`.../javac.properties`) under CratonVM. This is the same class of gap as the
recent `ClassLoader.getResourceAsStream` work for `java.base` boot classes — the
`jdk.compiler` module's packaged resources are not served to the running
compiler, so every diagnostic degrades to "compiler message file broken", and
H2 surfaces that string as a SQL syntax error.

## Next steps
- Verify `ToolProvider.getSystemJavaCompiler()` returns a working compiler under
  CratonVM and probe `getClass().getResourceAsStream(
  "/com/sun/tools/javac/resources/compiler.properties")`.
- Extend boot/module resource loading to cover `jdk.compiler` resources (mirror
  the `java.base` getResourceAsStream path).

## Repro
`CREATE ALIAS MY_SQRT FOR ...` with a Java-source alias, or any of the affected
test classes.
