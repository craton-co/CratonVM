# `java.nio.file.Path.toString()` dead-dispatched to `Object.toString()` — breaks in-process javac — FIXED

**Status: FIXED — 2026-07-19**

## Symptom

`module/spring-boot-web-server`'s `ServletComponentScanRegistrarTests
#processAheadOfTimeDoesNotRegisterServletComponentRegisteringPostProcessor()`
failed reproducibly (3/3 runs, not flaky):

```
org.springframework.core.test.tools.CompilationException: Unable to compile source

Errors:
- cannot find symbol
  symbol:   class Generated
  location: package org.springframework.aot.generate
- cannot find symbol
  symbol:   class BeanDefinition
  location: package org.springframework.beans.factory.config
- cannot find symbol
  symbol:   class RootBeanDefinition
  location: package org.springframework.beans.factory.support
```

This is Spring's `TestCompiler` (`org.springframework.core.test.tools`)
compiling AOT-generated bean-registration source in-process via the real
`javax.tools.JavaCompiler`. Discovered incidentally while regression-testing
an unrelated fix (see [[project_urlclassloader_getresourceasstream_20260718]]).

## Root-cause narrowing

Ruled out `@CompileWithForkedClassLoader` (the test's forked-classloader
annotation) and `TestCompiler`/`DynamicJavaFileManager` entirely: a minimal
standalone reproduction (`compiler.getTask(...)` with a plain
`StandardJavaFileManager`, no Spring test infrastructure at all) failed
identically compiling any source that imports an ordinary application
classpath class (`org.springframework.beans.factory.config.BeanDefinition`),
while the same setup compiling `java.util.List` (a platform/bootclasspath
class) succeeded. `fm.list(CLASS_PATH, ...)` alone (outside an active compile
task) also succeeded, finding all 96 expected class files — the difference
was isolated to `StandardJavaFileManager.inferBinaryName(CLASS_PATH, jfo)`,
which returned the literal string `java.nio.file` for **every** classpath
class file, instead of the actual binary name.

## Root cause

Two independent, compounding gaps:

1. **`java.nio.file.Path` is a genuine interface with no `toString()` body of
   its own.** Real virtual-dispatch resolution for a synthetic Path object's
   `toString()` call walks up to `java.lang.Object` (the only class in the
   hierarchy that actually declares `toString()` with a Code attribute) as
   the resolved declaring class. Neither `force_native_over_real_jdk_bytecode`
   (`vm/src/runtime/interpreter.rs`) nor the parallel `check_override`
   allow-list (`vm/src/vm/vm_exec.rs`) had an entry keyed on
   `java/nio/file/Path` itself, so that declaring-class match never fired —
   real `Object.toString()` ran instead of the registered native
   (`native-builtins::phases_late::register_phase57_nio_file`'s
   `Path.toString()`), producing the useless default form
   `java.nio.file.Path@<hash>` instead of the actual jar-FS/host path.
   (`redefine_immune_path_native` had already anticipated this exact
   (class, method) pair for the Mockito-redefine-immunity check, but the
   actual force-native entry that makes it relevant was never added.)

2. **`JavacFileManager.inferBinaryName`'s native fast path
   (`native_javac_file_manager_infer_binary_name` in `lib.rs`) read the
   `PathFileObject$JarFileObject`'s `path` field via
   `ctx.invoke_virtual(path, "toString", ...)`** — called from native Rust
   code rather than the bytecode interpreter, this never consulted either
   force-native gate above in the first place, so fixing gap 1 alone would
   not have fixed this call site.

Combined, every `PathFileObject$JarFileObject` classpath entry's binary name
resolved to the garbage string `java.nio.file` (`Object.toString()`'s
`java.nio.file.Path@<hash>` mangled by `javac_binary_name_from_relative_path`,
which strips everything after the last `.`), so real in-process javac could
never match a resolved classpath symbol back to its actual class — "cannot
find symbol" for every ordinary (non-JRT, non-directory) classpath class,
even basic framework classes.

## Fix

- `vm/src/runtime/interpreter.rs` (`force_native_over_real_jdk_bytecode`) and
  `vm/src/vm/vm_exec.rs` (`check_override` allow-list): add a
  `java/nio/file/Path` + `toString` entry to each.
- `native-builtins/src/phases_late.rs`: extracted the `Path.toString()`
  native's display-string logic into a shared `pub(crate) fn
  p57_path_display_string`.
- `native-builtins/src/lib.rs`: `native_javac_file_manager_infer_binary_name`'s
  `JarFileObject` branch now calls `p57_path_display_string` directly instead
  of `ctx.invoke_virtual(path, "toString", ...)`.

## Verification

Fixture: `apps/spring-boot` (Spring Boot 4.1.0-SNAPSHOT)
`module/spring-boot-web-server`, built on the Linux Azure host, run against
`cratonvm-scsraot-20260718` built from worktree
`/data/data/wt-scsregistrar-aot-20260718` (branch
`fix/scsregistrar-aot-testcompiler-20260718`, `dev` @ `a5eceb4de`).

`ServletComponentScanRegistrarTests`: 12/12 passing (up from 11/12), in both
JIT and `--nojit` (interpreter-only) mode.

**Regression sweep**: all 29 test classes in `module/spring-boot-web-server`
show no new failures. One unrelated pre-existing failure remains
(`WebServerSslBundleTests`, 3 failed — tracked by
[`webserversslbundletests-pkcs12-mac-verification-failure.md`](../../known-issues/springboot/webserversslbundletests-pkcs12-mac-verification-failure.md)).

This closure supersedes the "unrelated to the fix that surfaced it" framing
in [[project_urlclassloader_getresourceasstream_20260718]] — the two bugs
were independent all along, just discovered in the same regression sweep.
