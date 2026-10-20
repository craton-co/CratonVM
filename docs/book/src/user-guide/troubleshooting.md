# Troubleshooting

Common issues and how to resolve them. For build-from-source issues, see also
[Building from Source](../contributing/building.md).

## Runtime issues

### `ClassNotFoundException` / `NoClassDefFoundError`

The class isn't on the classpath. Pass `--classpath` with the directories and
JARs that contain it:

```bash
cratonvm --classpath "build/classes:lib/dependency.jar" com.example.Main
```

Remember: class **names** use dots (`com.example.Main`), while classpath
**entries** are directories/JAR paths. The separator is `;` on Windows and `:`
on Unix.

### `OutOfMemoryError`

Raise the heap:

```bash
cratonvm --Xmx 2g --classpath . MyApp
```

Inside a container, the ergonomic default is based on host RAM, so set `-Xmx`
explicitly — see [Containers & cgroups](containers.md). To capture a heap dump
on OOM, add `-XX:+HeapDumpOnOutOfMemoryError`.

### "native method not found" / `InternalError` / `NoSuchMethodError`

Your code uses a standard-library class or method CratonVM hasn't implemented
yet. Confirm and scope it with the missing-natives audit:

```bash
cratonvm --XX:AuditMissingNatives --classpath . MyApp
```

See [Finding missing standard-library
methods](debugging.md#finding-missing-standard-library-methods) and [Known
Limitations](../java-support/limitations.md).

### The program appears to hang

1. Re-run with a stack-dump watchdog to see where it's stuck:
   ```bash
   cratonvm --stack-dump-on-timeout 30 --classpath . MyApp
   ```
2. Re-run with `--nojit`. If the hang disappears, it points at a JIT issue.
3. Note that an allocation-heavy program with too small a heap can *look* like a
   hang while it thrashes the collector — try a larger `-Xmx`.

### Incorrect output or wrong computation

1. Compare the output with a standard `java` (HotSpot) run to confirm it's a
   CratonVM-specific difference.
2. Re-run with `--nojit` to localize the issue to the interpreter or the JIT.
3. Re-run with `--synthetic-jdk` vs. real-JDK mode to see whether the
   standard-library backend matters.
4. File a bug with the Java source and expected-vs-actual output (see below).

### `StackOverflowError` on deep recursion

This is the Java call stack, not the heap. Raise the limits:

```bash
RUST_MIN_STACK=8388608 RJ_MAX_STACK_DEPTH=8192 cratonvm --classpath . MyApp
```

## Build issues

### "linker `link.exe` not found" (Windows)

Install the Visual Studio Build Tools with the "C++ build tools" workload.

### "javac: command not found"

A JDK (17+) is needed only to compile Java sources/test classes. Install one
(for example from [adoptium.net](https://adoptium.net/)) and ensure `javac` is
on your `PATH`. CratonVM itself runs without a JDK.

### Stack overflow during `cargo test`

Some tests exercise deep recursion. Give them a larger stack:

```bash
RUST_MIN_STACK=8388608 cargo test --all -- --test-threads=4
```

## Reporting bugs

A good report includes:

1. A minimal Java reproduction (source and `.class`).
2. The exact command line.
3. Expected vs. actual output.
4. Whether it reproduces under `--nojit` and/or `--synthetic-jdk`.
5. `cratonvm --version`, your OS, and Rust version.

File issues on the project's
[GitHub issue tracker](https://github.com/craton-co/cratonvm/issues).
