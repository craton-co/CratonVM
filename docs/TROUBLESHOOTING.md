# Troubleshooting

Common issues and solutions when using CratonVM.

## Build Issues

### "linker `link.exe` not found" (Windows)

Install Visual Studio Build Tools with the "C++ build tools" workload.

### "javac: command not found"

JDK 17+ is required for compiling test Java classes. Install from
[adoptium.net](https://adoptium.net/) and ensure `javac` is on your PATH.
Tests that require `javac` will skip gracefully if it is not available.

### Stack overflow during tests

Some tests exercise deep recursion. Set a larger stack:

```bash
RUST_MIN_STACK=8388608 cargo test --all -- --test-threads=4
```

## Runtime Issues

### ClassNotFoundException

Ensure the class file is on the classpath. Use `--classpath` to specify directories
and JAR files:

```bash
cratonvm --classpath path/to/classes:lib/dependency.jar com.example.Main
```

Class names use dots (`com.example.Main`), not slashes or file paths.

### OutOfMemoryError

Increase the heap size with `--Xmx`:

```bash
cratonvm --Xmx 1g --classpath . LargeProgram
```

Default heap is 256 MB.

### "native method not found" / InternalError

CratonVM implements a subset of the Java standard library. If your code uses
a class or method that is not yet implemented, you will see this error.
Check [README.md](../README.md) for the list of supported classes.

### JIT compilation hangs

If a method takes too long to JIT-compile, the program may appear frozen.
Run with `--nojit` to disable JIT compilation and confirm the issue:

```bash
cratonvm --nojit --classpath . MyProgram
```

### Incorrect output / computation errors

1. Compare output with `java` (HotSpot) to confirm a CratonVM-specific issue.
2. Run with `--nojit` to check if the issue is in the interpreter or JIT.
3. Run with `--verbose:class` to see which classes are being loaded.
4. File a bug report with the Java source and expected vs actual output.

## GC Issues

### Frequent GC pauses

Use `--verbose:gc` to see GC activity:

```bash
cratonvm --verbose:gc --classpath . MyProgram
```

Increase heap size if the program needs more memory. The GC triggers when
from-space usage exceeds 75% capacity.

## Reporting Bugs

Include in your bug report:
1. Java source code (minimal reproduction)
2. Command line used
3. Expected vs actual output
4. CratonVM version (`cratonvm --version`)
5. OS and Rust version
