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
4. Optional, recommended — enable the repository's hooks:
   ```bash
   git config core.hooksPath .githooks
   ```
   Currently one `pre-push` hook, running the ~1.8 s flag-surface guards. CI
   already runs them, but branches here are merged into `dev` and pushed
   directly, so CI reports a red surface rather than preventing one. See
   [docs/contributing/flag-surface-hook.md](docs/contributing/flag-surface-hook.md).

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
All of those are blocking — including the exact synthetic-stub ratchet, which
runs in the ordinary `build-and-test` job and carries no `continue-on-error`.
Two things are deliberately advisory: the `Test vm (synthetic-jdk)` step,
because the harness aborts mid-run and cannot report a result at all, and the
`JDK-only mode` job, because wave 1 is measurement and a `--jdk-only` run is
still *expected* to fail on real workloads. Each carries its own comment with
the measurement and the conditions for re-promoting it. Nothing else in the
workflow is advisory; do not add to that list to make a branch green.

**Steps 1 and 4 are not green today.** `cargo fmt --all --check` reports over a
thousand diffs tree-wide, and `cargo test --workspace` has a residual failure
set that predates any given change and is tracked internally. Compare your run
against that known set rather than against zero, and note in the PR which
entries you saw — a *new* name in the output is the signal.

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
| `gc` | **ZGC is the default collector** since 2026-08-10 (`GcAlgorithm::Zgc` in `vm/src/config.rs`, behind the default-ON `zgc` feature in `gc/Cargo.toml`): `ZgcRealHeap` plus `zgc_concurrent.rs` and the twelve `src/zgc/` modules — colored pointers (`vaddr`), a load barrier (`barrier::z_load`), concurrent marking (`CRATONVM_ZGC_CONC_START`), compaction (`relocate`, kill switch `CRATONVM_ZGC_RELOCATE=0`) and an opt-in generational mode (`CRATONVM_ZGC_GENERATIONAL=1`). Generational (young/old; Cheney moving + non-moving sweep) stays available via `-XX:+UseGenerationalGC` and is the default in a `--no-default-features` build; G1 is opt-in via `-XX:+UseG1GC` (experimental) |
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
- ZGC production hardening — concurrent marking and compaction already exist and
  ZGC is the default collector; the open work is arming the colored-pointer load
  barrier, which is plumbed but inert (see [ROADMAP.md](ROADMAP.md))
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

3. **Declare an accurate `NativeKind` — this is mandatory, and the trap is that
   it is invisible at the call site.** `register()` takes four arguments and
   *none of them is the kind*. The kind is **ambient**: it comes from whatever
   `set_category` / `with_category` scope the enclosing registrar happens to be
   in, and the registry's `current_category` **defaults to
   `NativeKind::SyntheticStub`**. So a genuine bridge registered outside a
   `with_category(Bridge, …)` scope — or after a `set_category` that was never
   restored — is silently recorded as a stub. Wrap every new registration:
   ```rust
   registry.with_category(NativeKind::Bridge, |r| {
       r.register("java/lang/MyClass", "myMethod", "(Ljava/lang/String;)I", …);
   });
   ```
   Prefer the scoped `with_category` over bare `set_category`, and check the
   category actually in force at *your* call site rather than assuming the
   function you are editing sets one.

   This is not hypothetical. `native-api/src/registry.rs` carries a permanent
   diagnostic (`CRATONVM_DBG_DROPPED_STUBS`) added while chasing a real-JDK
   boot regression — `InternalError: null property: java.home` — that traced
   to a whole `register_*` function's worth of permanent
   `java.util.Properties` bridges inheriting the wrong ambient category at one
   of its call sites. Mis-tagging does not merely mislabel: under
   `CRATONVM_NO_STUBS`, and under `--jdk-only`, `register()` **refuses** a
   `SyntheticStub` outright, so a mis-tagged bridge is never registered at all
   and the failure surfaces far from its cause. The ambient default no longer
   decides anything: reclassification work closed out into five slack-free
   ratchets scored by `regression-suite/bridge-ratchet.sh` rather than a
   number in a document.

   One class of mis-tag is now decided centrally rather than at the site: a
   registration whose receiver class **no supported JDK image declares** cannot
   bind to an `ACC_NATIVE` method, so `register()` re-tags it `SyntheticStub`
   from the measured table in `native-api/src/no_image_receiver.rs`. If you are
   adding a native on a class the VM mints — an iterator stand-in, a functional
   combinator, a `cratonvm/…` receiver — check that table before choosing a
   kind; it is probably already deciding for you.

4. **Use `NativeContext`** — the `ctx` parameter provides:
   - `ctx.alloc_object(class_id)` — allocate a new object
   - `ctx.get_field(obj, index)` / `ctx.set_field(obj, index, value)` — field access
   - `ctx.get_string_value(obj)` — extract a Rust `String` from a Java String
   - `ctx.create_string(s)` — create a Java String from a Rust `&str`
   - `ctx.throw_exception(class, message)` — throw a Java exception

5. **Test it** — add a `#[test]` in the same file using `TestNativeContext` from
   `native-builtins/src/test_utils.rs`.

### How to Add a New `CRATONVM_*` Flag

A `CRATONVM_*` name touches **four files**, and the guards that enforce that are
`cargo test` assertions rather than compile errors — so `cargo build
--all-targets` stays green while any of them is missing. Editing two of the four
and stopping is how this has gone red before.

1. **`types/src/flag_groups.rs`** — an `E` row in `INVENTORY` (or a `SCALARS`
   entry). This is the only file that makes a name *declared*. The row carries a
   **`since:` date** in ISO `YYYY-MM-DD`; for a new knob that is today's date,
   and it is not a guess — if you cannot say when the name arrived, it arrived
   now. `tests::every_row_states_when_it_arrived` enforces the format.
2. **`types/tests/flag-surface.txt`** — the name, in sort order. Compared
   byte-for-byte, so match the file's existing line endings.
3. **`docs/flag-tokens.md`** — a `` | `token` | `KEY` | `` row in the group's
   section, and that section's "N tokens." count.
4. **`docs/config/flag-inventory.md`** — a Full-inventory row, the "N rows: D
   declared, A allowlisted." header, and the **declared** count in "Where the
   surface stands".

Files 3 and 4 are **generated**: `tools/flag-census/render-tokens.sh` and
`render-inventory.sh` write them from the table in file 1, and running them
beats hand-editing. `types/tests/flag_declaration_guard.rs` catches a literal
with no declaration, `flag_surface.rs` checks 1 against 2 in both directions,
and `flag_docs_generated.rs` checks 1 against 3 and 4 in both directions.

A name is not free-standing in either direction: a literal with no row fails the
guard, and a row with no read site fails check 5 of
`tools/flag-census/check-surface.sh`. **Land the declaration and its consumer in
the same change.** The read site must use `flags::runtime_var[_os]` — a raw
`std::env::var` on a declared name trips check 4 of the same script, because
declaring a name is what routes it through the latched snapshot that
`CRATONVM_DBG=token` and `flags::with_thread_overrides` reach.

**A `DBG` knob now has to justify its continued existence.**
`types/src/flag_groups.rs`'s own `mod tests` carries a retirement horizon: a
`DBG` row whose `since:` is on or after **2026-08-01** must be referenced
somewhere outside `types/`, the internal tree and the two generated flag
documents, or `a_dbg_knob_declared_since_the_horizon_has_a_live_consumer` fails
naming it. Either land the consumer or delete the row and its four-file
footprint. The horizon is measured, not chosen: it is the first month boundary
above the newest consumer-less `DBG` row, and moving it *earlier* is the
retirement work itself. The population it deliberately grandfathers is published
in [`docs/config/flag-retirement-candidates-20260901.md`](docs/config/flag-retirement-candidates-20260901.md)
— of 995 declared knobs, 488 had no operator- or CI-facing mention and 63 of
those had at most one Rust read site, which is where removal starts. Nothing in
this repository has ever removed a flag; the date field exists so that the first
removal can be argued rather than guessed.

### Adding a Compatibility Stub

A *compatibility stub* is anything that stands in for the JDK's own code: a
`NativeKind::SyntheticStub` native, or a class fabricated without real class
bytes. They are permitted, but they are debt, and debt has to be booked. A PR
that adds one must carry all three of:

1. **An explicit non-strict classification.** State in code that the thing is a
   stub — `with_category(NativeKind::SyntheticStub, …)` at the registration, or
   the corresponding `ClassOrigin::CompatibilityStub { reason }` with a real
   reason string. Do not let a stub reach that classification by *omission*: an
   unclassified registration already defaults to `SyntheticStub`, so "it came
   out tagged correctly" is not evidence that anybody decided.
2. **Tests.** Cover the behaviour the stub stands in for, so the day it is
   deleted the replacement is checked against something.
3. **A tracking issue for its removal**, linked from the code comment. A stub
   with no removal issue is a permanent divergence that nobody has agreed to.

**Do not raise the ratchet baseline to go green.** `cargo test -p
cratonvm-native-builtins --test stub_ratchet` asserts that the `SyntheticStub`
count never exceeds the committed baseline, and that baseline is frozen with
zero slack precisely so a single new application-visible stub fails CI.
Editing the baseline constant to match your branch converts a signal into a
rubber stamp. If the count legitimately has to move, the baseline change is the
subject of the PR and needs its own justification — not a line in a diff that is
about something else. See
[`docs/contributing/stub-ratchet.md`](docs/contributing/stub-ratchet.md) and the
[no-synthetic-stubs policy](docs/contributing/no-synthetic-stubs.md).

Before writing a stub, check whether the JDK's own bytecode can run instead;
[`docs/jdk-only-native-review.md`](docs/jdk-only-native-review.md) is the
checklist for that decision (and the gate every existing stub must pass to
survive into `--jdk-only`). Conversely, if you are implementing a genuine VM
boundary crossing or a proven intrinsic, tag it `Bridge` / `Intrinsic` — see
the ambient-category warning in step 3 above, because getting that wrong turns
a bridge into a stub silently.

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
