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

---

## RESOLUTION (2026-06-29) — in-process javac now compiles end-to-end

**Status: FIXED.** `javax.tools.ToolProvider.getSystemJavaCompiler()` compiles a
real source string, and H2's `CREATE ALIAS findOneUser AS $$ … $$`
(`SessionDelegatorBaseImplTest`) succeeds — `compile(Good) ok=true`,
`PART2: CREATE ALIAS OK`, byte-comparable to HotSpot on the standalone probes.

The bug had **far more layers** than the message bundle. The earlier
"FIXED" note (`HIB-CV-27-javac-message-bundle-class-based-listresourcebundle.md`)
verified only a probe that compiled a *deliberately broken* snippet, which
errors at **parse time** — so it never exercised symbol resolution and the real
defects below stayed hidden. Layers, in the order a real compile hits them:

1. **Message bundle = class-based `ListResourceBundle`** (prior fix; correct).
2. **`getSystemJavaCompiler()` == null** → boot module registry population
   (prior fix; correct). NB: populating the registry was also what surfaced
   layer 3.
3. **Debug-only VM-init crash.** The eager boot-module scan reaches ~138
   modules with a large app classpath, tripping a stale
   `debug_assert(modules.len() <= 100)` in `build_readability_graph`
   (release compiles it out — why the gauntlet/probes missed it). Raised to
   4096. *(commit: module readability tripwire)*
4. **`FileSystem.getRootDirectories()` NPE for mounted jars.** javac's
   `ArchiveContainer` walks it; it returned a null-iterating `SingletonList`
   and a non-jar root → `NPE` in `SimpleFileVisitor.visitFile`. *(commit:
   getRootDirectories jar walk)*
5. **No `jrt:` FileSystem.** javac reads platform classes from the runtime
   image; `FileSystems.getFileSystem(jrt:/)` threw `ProviderNotFoundException`
   → "Unable to find package java.lang in platform classes". Added a synthetic
   jrt provider/FS backed by `JImageReader`.
6. **`Files.list`/`Files.walk` empty over jar/jrt** → javac package enumeration
   saw zero classes. Registered both as natives returning a working Stream.
7. **`Path.getFileName()` returned non-null `""` for a jar/jrt root** (instead
   of `null`) → javac `SKIP_SUBTREE`'d every classpath jar at its root →
   "package org.h2.tools does not exist". Now `null` for an encoded root.
8. **`Path.relativize` returned garbage for encoded paths** (component-wise
   `strip_prefix` over the sentinel strings) and rendered the host `\` → javac
   keyed its package map wrong. Now relativizes in entry-space and renders `/`.
9. **Perf:** the jar-FS helpers re-`read` each archive per directory visit —
   O(entries × jar-size); a jar-bytes cache makes walking a 241-jar classpath
   tractable.

Fixes are in `native-builtins/src/phases_late.rs` (+ a `make_stream_from_elements`
helper in `native-collections`). Probes:
`apps/hib-suite-runner/{Hib27Probe,JrtProbe,JavacCpProbe,NioWalkProbe}.java`.

### Phantom manifest `Class-Path:` siblings (FIXED here)

A jar whose manifest carries a `Class-Path:` header pointing at sibling jars
that are NOT present resolves those entries against the jar's own directory.
Apache Derby's `Class-Path: derbyshared.jar derbyLocale_*.jar …` is the canonical
case — none of those live in the Gradle cache's per-artifact hash dir, so each
resolves to a non-existent path. javac adds them to the classpath
(`FSInfo.getJarClassPath` → `tryResolveFile`, which does NOT check existence —
HotSpot adds them too) and later mounts each to list packages.

CratonVM's synthetic jar provider mounts a non-existent jar (it does not throw
like the JDK zip provider), and `jarfs_classify` reported the mounted root as
`Absent` → `readAttributes(root)` threw `NoSuchFileException` → javac surfaced it
as a fatal `cannot access <package>` (a missing jar contributes no classes, so
it must instead behave as an EMPTY container). Fix: `jarfs_classify` now reports
the mounted root as a directory even when the jar file is missing, so the walk
yields nothing and javac skips the entry — the net effect HotSpot gets by
throwing at mount time and skipping. (NB: this depends on `URI.toURL()` resolving
`file:///…` correctly, which dev's `fix(net): URI.toURL() parse authority …`
already does; the older pre-fix `toURL` mis-set the URL authority so the
`Class-Path` resolution threw `Illegal character found in authority: '/'` and the
entries were never added — i.e. the bug was previously *masked*.)

Verified: H2 `CREATE ALIAS` with Derby on the classpath compiles cleanly (no
diagnostic) and `SessionDelegatorBaseImplTest` passes. Probes:
`apps/hib-suite-runner/{UrlChain,DerbyDiag,UriProbe,JarWalkJavac}.java`.

**Remaining minor divergence (cosmetic, not needed for the test):** CratonVM's
`URI.toURL()` for a `file:///…` URI still renders it as `file:///…` rather than
collapsing to `file:/…` (and a hand-rolled `file:/…` URL built by the `toURL`
native reports its slot-5 `authority` as the whole spec rather than null). It
does not break the in-process compiler, but is worth aligning with HotSpot in a
focused follow-up.
