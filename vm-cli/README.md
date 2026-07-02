# cratonvm-cli

Command-line launcher for CratonVM — the `java`-equivalent front-end.

Part of the [CratonVM](https://github.com/craton-co/cratonvm) Java Virtual
Machine implemented from scratch in Rust.

## Scope

Ships the project-specific `cratonvm` binary by default. An additional
`java[.exe]` alias can be built from the same entry point with the
`java-bin-alias` Cargo feature when a tool requires the launcher basename to be
`java`. Parses JVM flags, builds a `VmConfig`, configures
classpath / jar / module-path, installs the mimalloc global allocator,
sets up tracing, and invokes `main(String[])` (or the manifest
`Main-Class` for `--jar`). The alias is intentionally off by default so
`cargo install cratonvm-cli` does not shadow the host JDK.

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
cargo build -p cratonvm-cli --features java-bin-alias
java -cp target/classes com.example.Hello   # optional alias, java-named
```

## Status

Pre-1.0. API stability is best-effort. Tied to the
[CratonVM](https://github.com/craton-co/cratonvm) workspace version.

## License

Apache-2.0. See `LICENSE` and `NOTICE` at the workspace root.

Copyright 2024-2026 Craton Software Company.
