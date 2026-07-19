# `java.nio.file.Path.toString()` dead-dispatched to `Object.toString()` via `invokedynamic` string concatenation

**Status: OPEN — found 2026-07-19**

## Symptom

`"file:" + somePath` (a `Path` operand in an `invokedynamic`-based Java
string concatenation, `StringConcatFactory.makeConcatWithConstants`)
stringifies the `Path` as `java.nio.file.Path@<hash>` (the default
`Object.toString()` format) instead of the real path text, e.g.:

```
14:12:43.265 [main] WARN ...DefaultTemplateResolverConfiguration -- Cannot find template location: file:java.nio.file.Path@6ddc2 (please add some templates...)
```

Hit in both `module/spring-boot-thymeleaf`'s `ThymeleafReactiveAutoConfigurationTests`
and `ThymeleafServletAutoConfigurationTests`, method `templateLocationEmpty(CapturedOutput, Path)`:

```java
void templateLocationEmpty(CapturedOutput output, @TempDir Path tempDir) throws IOException {
    Path directory = tempDir.resolve("empty-templates/empty-directory").toAbsolutePath();
    Files.createDirectories(directory);
    this.contextRunner.withPropertyValues("spring.thymeleaf.prefix:file:" + directory)
        .run((context) -> assertThat(output).doesNotContain("Cannot find template location"));
}
```

`"spring.thymeleaf.prefix:file:" + directory` concatenates a `Path` operand;
the resulting prefix string is malformed (`file:java.nio.file.Path@6ddc2`
instead of the real absolute path), so Thymeleaf's template-location check
looks at a bogus location and (correctly, given the bogus input) logs the
"Cannot find template location" warning the test asserts must NOT appear.

Full logs (`craton-rerun-verify-20260719`):
- `apps/spring-boot-suite-runner/.suite/results/thymeleaf-groovy-fix-verify-20260719/all-jit/logs/module_spring-boot-thymeleaf.org.springframework.boot.thymeleaf.autoconfigure.ThymeleafReactiveAutoConfigurationTests.out.log`
- Same shape independently reproduced against `ThymeleafServletAutoConfigurationTests` (26/26 non-hanging tests run — see [`thymeleaf-groovy-layoutdialect-metaclass-introspection-hang.md`](thymeleaf-groovy-layoutdialect-metaclass-introspection-hang.md) for why the class needs its `createLayoutFromConfigClass` test excluded to get a clean run today) with an identical failure and message shape.

## Root cause

**Confirmed family, distinct call site from the already-fixed one.** This is
the same class of bug as `docs/internal/springboot/path-tostring-dead-dispatch-breaks-inprocess-javac-FIXED.md`
(`java.nio.file.Path` is an interface with no `toString()` body of its own;
CratonVM's registered native `Path.toString()` must be force-native-gated in
every dispatch path that can reach it, or real dispatch resolves to
`java.lang.Object.toString()` and prints `ClassName@hash`). That fix closed
two specific gaps: the interpreter's `invokevirtual` force-native gate, and
one native-to-native `ctx.invoke_virtual` call site inside
`native_javac_file_manager_infer_binary_name`.

This is a **third, still-open gap**: Java 9+ compiles `"literal" + path`
via `invokedynamic`/`StringConcatFactory.makeConcatWithConstants`, not
`StringBuilder.append(Object)` bytecode. Whatever code backs that
`invokedynamic` bootstrap in CratonVM (likely `vm/src/runtime/invokedynamic.rs`
or a dedicated string-concat native, stringifying each concatenated operand)
needs to be audited the same way the `TestCompiler`/`javac` fix doc
describes for its own call site — it is very likely calling something that,
like `ctx.invoke_virtual`, bypasses the interpreter-level force-native gate
and falls through to `Object.toString()`. Not yet isolated to a specific
function/line — whoever picks this up next should grep the
`StringConcatFactory`/indy-concat bootstrap implementation for how it
stringifies each argument and compare against the two already-fixed call
sites' pattern.

## Reproduction

A minimal repro is simplest as a one-off: any `"literal" + aPathInstance`
expression compiled with a modern `javac` (indy string concat is the
default since Java 9) and run under CratonVM will print
`java.nio.file.Path@<hash>` instead of the real path. The Thymeleaf test
above is a convenient existing repro; no standalone minimal case has been
authored yet.

## Affected classes

- `module/spring-boot-thymeleaf` | `ThymeleafReactiveAutoConfigurationTests` | `templateLocationEmpty(CapturedOutput, Path)`
- `module/spring-boot-thymeleaf` | `ThymeleafServletAutoConfigurationTests` | `templateLocationEmpty(CapturedOutput, Path)`
