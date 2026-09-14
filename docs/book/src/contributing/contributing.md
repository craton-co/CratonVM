# Contributing Guide

Thanks for your interest in contributing to CratonVM. This chapter covers the
workflow and the two most common kinds of change: adding a bytecode opcode and
adding a native method. See [Building from Source](building.md) and
[Testing](testing.md) for the mechanics.

## Code of conduct

The project follows the Contributor Covenant Code of Conduct (`CODE_OF_CONDUCT.md`
in the repository). By participating, you agree to uphold it.

## Workflow

1. Fork the repository and clone your fork.
2. Install the prerequisites and build/test (see [Building](building.md)).
3. Make your change on a branch.
4. Run the release-quality checks and record any known failures in the PR notes.
   CI is configured to run these on every push and PR:
   - **Format:** `cargo fmt --all --check`
   - **Lint:** `cargo clippy --workspace --all-targets -- -D warnings`
   - **Build:** `cargo build --workspace`
   - **Test:** `cargo test --workspace`
5. Open a pull request against `main` and fill out the PR template.
6. Address review feedback with follow-up commits (rather than force-pushing).

### Code style

- `rustfmt` defaults with `max_width = 100`.
- Keep `cargo clippy --workspace --all-targets -- -D warnings` clean under the
  workspace lint config; do not claim zero warnings unless that command passes
  on the branch being submitted.
- Use `thiserror` for error types and `tracing` for logging (not `println!` /
  `eprintln!` in library crates).
- Add a `// SAFETY:` comment to every `unsafe` block explaining the invariant.

### Changelog & sign-off

- Add a one-line entry under `Unreleased` in the changelog, in the right
  subsection (`Added`, `Changed`, `Fixed`, `Removed`, or `Performance`).
- The project uses the **Developer Certificate of Origin**. Sign off your
  commits with `git commit -s`, which appends a `Signed-off-by:` line.

## Adding a bytecode opcode

1. **Define it** — add a variant to `Instruction` in
   `reader/src/instruction.rs` (the opcode byte and its operands).
2. **Parse it** — add a decode arm in the reader's `read_instruction()`.
3. **Verify it** — add a verification case in the classloading verifier that
   checks the expected stack/local types and the output type.
4. **Interpret it** — add a match arm in the interpreter's dispatch loop, following
   the existing stack-manipulation patterns.
5. **JIT-compile it** (optional) — add code generation in the x86-64 emitter.
6. **Test it** — add interpreter unit tests and a Java test class in
   `test_classes/` that exercises the opcode.

## Adding a native method

1. **Choose the crate** — `native-builtins` for `java.lang.*`,
   `native-collections` for `java.util.*`, `native-io` for `java.io`/`java.nio`.
2. **Register it** in that crate's registration function:

   ```rust
   registry.register(
       "java/lang/MyClass",
       "myMethod",
       "(Ljava/lang/String;)I",            // descriptor
       |ctx, args| {
           // args[0] = this (for instance methods); args[1..] = parameters
           let s = ctx.get_string_value(args[1].as_object().unwrap().unwrap())?;
           Ok(Some(Value::Int(s.len() as i32)))
       },
   );
   ```

3. **Use `NativeContext`** for heap/field/string/exception access (see [Native
   Methods](../internals/native-methods.md)).
4. **Test it** with a `#[test]` using the test `NativeContext` helper in the
   crate.

> **Design constraint:** prefer running real `.class` files from the JDK and
> application classpath over synthetic stub classes for application-visible
> types. Synthetic stubs are a standalone fallback, not the goal.

## Good first contributions

- Adding missing `java.lang.Math` methods.
- Improving bytecode-verifier error messages.
- Adding edge-case tests for existing native methods.

Larger projects (AArch64 JIT parity, GC throughput, fuller JNI, module-system
completeness) are tracked in the [Roadmap](roadmap.md).

## Reporting issues

Use the project's GitHub issue tracker. Include the Java source (and `.class`,
or steps to reproduce), the exact command line, and the full error output.
Security issues should be reported privately per the repository's `SECURITY.md`,
not as public issues.
