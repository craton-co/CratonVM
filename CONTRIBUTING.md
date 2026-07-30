# Contributing to CratonVM

Thank you for your interest in contributing to CratonVM! This document provides guidelines
and information to help you get started.

## Code of Conduct

This project follows the [Contributor Covenant Code of Conduct](CODE_OF_CONDUCT.md).
By participating, you are expected to uphold this code.

## Getting Started

1. Fork the repository and clone your fork
2. Install prerequisites (see [BUILD_GUIDE.md](BUILD_GUIDE.md)):
   - Rust 1.80+ via [rustup.rs](https://rustup.rs)
   - JDK 17+ (for compiling test Java classes)
   - Visual Studio Build Tools (Windows only)
3. Build and run tests:
   ```bash
   cargo build --all-targets
   cargo test --all
   ```

## Development Workflow

### Before Submitting

Run the following release-quality checks before submitting and include any
known failures in the PR notes. CI (`.github/workflows/ci.yml`) is configured to
run them on every push and pull request:

1. **Format** — `cargo fmt --all --check`
2. **Lint** — `cargo clippy --workspace --all-targets -- -D warnings`
3. **Build** — `cargo build --workspace`
4. **Test** — `cargo test --workspace`

CI runs these on `ubuntu-latest` and `windows-latest`, together with the
`synthetic-jdk` and `experimental-*` feature gates, the exact synthetic-stub
ratchet, the Markdown link check, the semantic differential gate, the fuzz build
smoke, coverage generation, and a Miri job over the core representation crate.
None of them are `continue-on-error`.

**Step 4 is not green today.** `cargo test --workspace` has a residual failure
set that predates any given change, tracked in
[`docs/known-issues/jit-regressions-hidden-by-unbuildable-test-targets-20260730.md`](docs/known-issues/jit-regressions-hidden-by-unbuildable-test-targets-20260730.md).
Compare your run against that list rather than against zero, and note in the PR
which entries you saw — a *new* name in the output is the signal.

### Code Style

- Follow `rustfmt` defaults with `max_width = 100` (see `rustfmt.toml`)
- Keep `cargo clippy --workspace --all-targets -- -D warnings` clean under the
  workspace `[lints]` config (the root `Cargo.toml` `allow`s
  `dead_code`/`unused_*` and a few rustdoc lints), not the full default lint set
- Use `thiserror` for error types
- Use `tracing` for logging (not `println!` or `eprintln!` in library crates)
- Add `// SAFETY:` comments to all `unsafe` blocks explaining the invariant

### Project Structure

| Crate | Purpose |
|-------|---------|
| `reader` | Java `.class` file parser |
| `types` | Shared types (Value, ClassId, ObjectRef) |
| `native-api` | NativeContext trait & FD table |
| `native-builtins` | java.lang.* native methods |
| `native-collections` | java.util.* native methods |
| `native-io` | java.io/nio native methods |
| `native-awt` | AWT/Swing/Java2D native peer implementation |
| `jit-api` | JIT compiler API types |
| `jit` | x86-64 / AArch64 JIT compiler |
| `jit-cuda` | Java bytecode -> PTX lowering for GPU offload |
| `cuda-bridge` | Thin CUDA Driver API bridge for GPU offload |
| `craton-gpu` | Build-time Java annotation sources (`@Parallel` etc.) for GPU offload |
| `classloading` | Class loading & bytecode verification |
| `gc` | Generational GC default (young/old; Cheney moving + non-moving sweep); opt-in G1 region collector (`-XX:+UseG1GC`, experimental); feature-gated `zgc` stub |
| `jfr` | Java Flight Recorder |
| `vm` | VM runtime engine |
| `vm-cli` | Command-line entry point |
| `libcratonvm` | C-ABI shared library for embedding (cdylib/staticlib `libjvm` substitute, JNI Invocation API) |
| `cratonvm-embed` | Curated, semver-stable Rust facade for embedding CratonVM |

### Writing Tests

- Add unit tests in the same file as the code (`#[cfg(test)]` module)
- Integration tests that require `javac` should skip gracefully if it's not available
- Use `RUST_MIN_STACK=8388608` for tests involving deep recursion

### Commit Messages

- Use concise, descriptive commit messages
- Prefix with the affected area when useful: `reader: fix constant pool bounds check`
- Reference issue numbers where applicable: `Fix #42: handle empty switch tables`

### Updating the Changelog

When your PR adds a feature, fixes a bug, or changes behavior:

1. Add an entry under `## [Unreleased]` in [CHANGELOG.md](CHANGELOG.md)
2. Use the appropriate subsection: `Added`, `Changed`, `Fixed`, `Removed`, or `Performance`
3. Keep entries concise (one line per change)

## Code Review Process

1. Open a pull request against `main`
2. Fill out the [PR template](.github/pull_request_template.md)
3. A maintainer will review your PR, typically within a few days
4. Address any feedback — push follow-up commits rather than force-pushing
5. Once approved and CI passes, a maintainer will merge the PR

## Areas for Contribution

### Good First Issues

- Adding missing `java.lang.Math` methods
- Improving error messages in the bytecode verifier
- Adding tests for edge cases in existing native method implementations

### Larger Projects

- ARM64 JIT backend (`jit/src/aarch64.rs`)
- Concurrent garbage collector
- Full JNI implementation
- Module system support

### How to Add a New Bytecode Opcode

1. **Define the opcode** — add a variant to `Instruction` in `reader/src/instruction.rs`.
   Include the opcode byte value and any operands.

2. **Parse it** — in `reader/src/class_reader.rs`, add a decode arm in `read_instruction()`
   that reads the operand bytes and constructs your `Instruction` variant.

3. **Verify it** — in `classloading/src/verify_insn.rs`, add a verification case that
   checks the expected stack/local types and produces the correct output type.

4. **Interpret it** — in `vm/src/runtime/interpreter.rs`, add a match arm in the main
   dispatch loop. Follow the existing patterns for stack manipulation.

5. **JIT-compile it** (optional) — in `jit/src/x64.rs`, add code generation for the
   new opcode in `compile_instruction()`.

6. **Test it** — add unit tests in the interpreter module and a Java test class in
   `test_classes/` that exercises the opcode.

### How to Add a New Native Method

1. **Choose the crate** — `native-builtins` for `java.lang.*`, `native-collections`
   for `java.util.*`, `native-io` for `java.io.*`/`java.nio.*`.

2. **Register the method** — in the appropriate crate's registration function, add:
   ```rust
   registry.register(
       "java/lang/MyClass",
       "myMethod",
       "(Ljava/lang/String;)I",   // descriptor
       |ctx, args| {
           // args[0] = this (for instance methods)
           // args[1..] = parameters
           let s = ctx.get_string_value(args[1].as_object().unwrap().unwrap())?;
           Ok(Some(Value::Int(s.len() as i32)))
       },
   );
   ```

3. **Use `NativeContext`** — the `ctx` parameter provides:
   - `ctx.alloc_object(class_id)` — allocate a new object
   - `ctx.get_field(obj, index)` / `ctx.set_field(obj, index, value)` — field access
   - `ctx.get_string_value(obj)` — extract a Rust `String` from a Java String
   - `ctx.create_string(s)` — create a Java String from a Rust `&str`
   - `ctx.throw_exception(class, message)` — throw a Java exception

4. **Test it** — add a `#[test]` in the same file using `TestNativeContext` from
   `native-builtins/src/test_utils.rs`.

See [ROADMAP.md](ROADMAP.md) for the full list of planned work.

## Reporting Issues

- Use [GitHub Issues](https://github.com/craton-co/cratonvm/issues) for bug reports and feature requests
- Include the Java source code and `.class` file (or steps to reproduce) for bugs
- Include the full error output from CratonVM

## Developer Certificate of Origin (DCO)

This project uses the [Developer Certificate of Origin](https://developercertificate.org/) (DCO).
By submitting a pull request, you certify that your contribution is your original work
(or you have the right to submit it) and that you agree to license it under the project's
Apache 2.0 license.

You can sign off your commits by adding `-s` to `git commit`:

```bash
git commit -s -m "reader: add support for ConstantDynamic"
```

This appends a `Signed-off-by: Your Name <email@example.com>` line to your commit message.

## License

By contributing, you agree that your contributions will be licensed under the
[Apache License 2.0](LICENSE).
