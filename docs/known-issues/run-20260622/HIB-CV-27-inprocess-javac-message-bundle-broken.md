# HIB-CV-27 — In-process Java compiler fails: "compiler message file broken" (resource-bundle loading)

**Run:** full Hibernate ORM suite, 2026-06-22/23
**Binary:** `cvhibtest.exe` (dev `c863b23e`)
**Severity:** Medium-High — breaks any runtime use of `javax.tools.JavaCompiler`; **deterministic, `--nojit`**, HotSpot PASS
**Status:** Confirmed; root area = `jdk.compiler` resource-bundle / module-resource loading

---

## Symptom

`org.hibernate.orm.test.delegation.SessionDelegatorBaseImplTest` (H2 `CREATE ALIAS`
stored procedure, which compiles a Java snippet via the in-process compiler):

```
org.hibernate.exception.SQLGrammarException: Error executing work
  [Syntax error in SQL statement "compiler message file broken:
   key=compiler.err.error arguments={0}, {1}, {2}, {3}, {4}, {5}, {6}, {7}
   compiler message file broken: key=compiler.err.error.reading.file
   arguments=C:\Users\...\.gradle\caches\...org.apache.d... "]
```

The H2 alias triggers `com.sun.tools.javac` (the `jdk.compiler` module). javac then
emits **"compiler message file broken: key=compiler.err.error…"** — its diagnostic
**`ResourceBundle` (`compiler.properties`) could not be loaded**, so it cannot even
format its own error messages. HotSpot: PASS (compiles the alias fine).

## Why it's a real CratonVM bug

- Deterministic, reproduces standalone `--nojit` (not the JIT family).
- HotSpot PASS.

## Root cause area

"compiler message file broken: key=…" is javac's fallback when
`ResourceBundle.getBundle("com.sun.tools.javac.resources.compiler")` fails. On
CratonVM the `jdk.compiler` module's properties resources are **not loadable at
runtime** — i.e. CratonVM does not serve `*.properties` resources from a JDK
module (or `ResourceBundle`/`Module.getResourceAsStream` for `jdk.compiler` returns
null). Any program that runs the Java compiler in-process (annotation processors,
JSP/expression compilers, H2/HSQLDB Java stored procedures, dynamic code gen)
will be affected.

## Reproduce

```
cvhibtest.exe --java-home <jdk25> --nojit @common.args -Dcraton.trace=1 \
  CratonRunner <list-with-SessionDelegatorBaseImplTest> 0
# -> SQLGrammarException ... "compiler message file broken: key=compiler.err.error ..."
```

A tighter repro: call `javax.tools.ToolProvider.getSystemJavaCompiler()` and
compile a trivial source string; the diagnostics will show the broken message
bundle.

## Suggested next step for a fixer

Check CratonVM's resource loading for JDK **module** resources — specifically
`.properties` under `jdk.compiler` (`com/sun/tools/javac/resources/*.properties`).
Verify `ResourceBundle.getBundle(...)` and the system/platform classloader's
`getResourceAsStream` return the module's property files.

## Triage

Real, deterministic, independent of the JIT. Affects all in-process compilation.
Hand off to whoever owns module/resource loading.
