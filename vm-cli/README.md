# cratonvm-cli

Command-line launcher for CratonVM — the `java`-equivalent front-end.

Part of the [CratonVM](https://github.com/craton-co/cratonvm) Java Virtual
Machine implemented from scratch in Rust.

## Scope

Ships two binaries (`cratonvm` and `java`, both built from the same
entry point) that load and run Java bytecode the way the stock `java`
launcher does. Parses JVM flags, builds a `VmConfig`, configures
classpath / jar / module-path, installs the mimalloc global allocator,
sets up tracing, and invokes `main(String[])` (or the manifest
`Main-Class` for `--jar`). The `java` binary alias exists so tools like
Maven Surefire that validate the launcher path (`*/java` or
`*/java.exe`) accept CratonVM as a JVM.

## Non-goals

- No VM implementation logic — every responsibility beyond CLI parsing
  belongs to `cratonvm-vm`.
- No build-system integration (no Maven plugin, no Gradle plugin).
- Not a library: this crate exists to produce executables. Embedders
  should depend on `cratonvm-vm` directly.

## Usage

```bash
cratonvm com.example.Hello arg1 arg2
cratonvm --jar app.jar arg1 arg2
java -cp target/classes com.example.Hello   # same binary, java-named
```

## Status

Pre-1.0. API stability is best-effort. Tied to the
[CratonVM](https://github.com/craton-co/cratonvm) workspace version.

## License

Apache-2.0. See `LICENSE` and `NOTICE` at the workspace root.

Copyright 2024-2026 Craton Software Company.
