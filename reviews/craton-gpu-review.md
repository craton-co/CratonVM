# craton-gpu review

## Summary

- **LOW.** Scope mismatch with the audit brief: this crate is *not* the
  "higher-level GPU compute layer" the audit prompt described. It is a
  ~16-line Rust shim around a `build.rs` whose only job is to compile a
  handful of Java annotation source files (`@GpuKernel`, `@GpuExclude`,
  `@EnableGpuAsync`) into a JAR and surface two paths to dependents via
  cargo `links` metadata. There is no Rust runtime logic, no GPU code,
  no CUDA, no kernels. The README at `craton-gpu\README.md:1-46` is
  explicit and accurate about this.
- **MED.** The Java sources the build script compiles do not live in
  this repository at all. `build.rs:39-45` resolves them via
  `$CRATON_GPU_JAVA_SRC`, `../craton-gpu-java/src/main/java`, or
  `C:/craton/craton-gpu-java/src/main/java`. None of these exist on the
  current checkout. The build script silently falls through with an
  empty annotations JAR, which means a vanilla clone produces a
  no-annotations build — and there is no integration test that catches
  the regression.
- **MED.** Path is hard-coded to a Windows-style drive
  (`C:/craton/craton-gpu-java/...`, `build.rs:182`). Acceptable as a
  developer convenience but undocumented in README/OSS messaging and is
  the only Windows-specific path in the crate.
- **LOW.** Build script is resilient (every error path is a
  `cargo:warning=`, never a fail), SPDX header is present on `lib.rs`,
  trademark file at workspace root already covers NVIDIA/CUDA/PTX.
- **MED.** Zero unit/integration tests in the crate; coverage is
  effectively 0%. The crate is exercised indirectly by `jit-cuda`'s
  build.rs, but nothing pins the `DEP_CRATON_GPU_ANNOTATIONS_*` contract.

## 1. Code review

### Bugs

- **MED — silent empty-build on clean checkouts.** `build.rs:173-190`
  returns the *last* candidate path (`C:/craton/craton-gpu-java/...`)
  even when nothing exists. `main()` then calls `java_root.is_dir()`
  (`build.rs:98`) which is false, `sources` stays empty, and
  `emit_env("", &classes_dir)` is invoked at `build.rs:121`. The result
  is a successful build with empty `ANNOTATIONS_JAR` / `ANNOTATIONS_DIR`.
  No README/CHANGELOG entry alerts the user that a sibling repo must be
  cloned; no CI gate ensures it was. Anyone who clones CratonVM, builds
  it, and tries `@GpuKernel`-driven offload will silently see the
  annotation reader fail to find any classes.
- **LOW — `resolve_java_root` returns the C-drive candidate as a "default
  even when missing"** at `build.rs:189`. The intent (caller emits a
  warning) is fine, but the function's name and contract suggest it
  returns *something useful*. Consider returning `Option<PathBuf>` so
  callers handle the missing-source case explicitly.
- **LOW — race-prone `OUT_DIR` dependency.** `build.rs:50-53` does
  `expect("OUT_DIR is set by cargo")`. Fine for cargo, but the surface
  contract on this env var is that build scripts are *always* invoked
  by cargo. Not a bug.
- **LOW — `OsString::from(jar_path)` then `cmd.arg(...)`** at
  `build.rs:224`. Redundant: `cmd.arg(jar_path)` accepts `&Path`
  directly. Stylistic, not functional.
- **LOW — `collect_java` recursion has no depth cap.** `build.rs:203-217`
  recurses into every subdirectory under the source root. A symlink loop
  would cause a stack overflow. Low practical risk because the source
  root is a known-shape Maven project; consider `walkdir` with
  `follow_links(false)` if hardening is desired.
- **LOW — UTF-8 assumption.** `build.rs:84` writes
  `cargo:annotations_dir={}` using `dir.display()`, which lossily
  replaces non-UTF-8 path components. On Windows this can mangle paths
  containing `\xFF`-range bytes. In practice `OUT_DIR` and the
  default-install path are ASCII, so this is theoretical.
- **NIT — `cargo:rerun-if-changed={}` for the whole `java_root`**
  (`build.rs:68`) does not actually fire on per-file edits — the comment
  at `build.rs:108-115` acknowledges this and adds per-file lines, but
  the wholesale dir line stays as well. Either form alone would do; the
  duplicate is harmless.

### Vulnerabilities

- None of substance. Crate runs `javac` and `jar` from `PATH` (no fixed
  absolute path); a hostile `javac` on the user's `PATH` could execute
  arbitrary code at build time, but this is the standard Java
  development model and the workspace-level `BUILD_GUIDE.md` covers it.
- No untrusted input is processed at runtime; the only constants the
  crate exports are absolute paths embedded by `env!()` from build.rs
  (`src\lib.rs:11,15`). These are propagated unchanged to dependents.

### Stubs

- No `todo!()`, `unimplemented!()`, `FIXME`, `XXX`, or `HACK` markers
  in either `build.rs` or `src\lib.rs`. There are also no functions
  whose body is a single `unreachable!()`.

### Performance

- N/A. The crate has zero runtime cost (the constants compile to
  string literals, `src\lib.rs:11-15`). The build script's only
  expensive call is `javac`, which compiles a handful of annotation
  classes once per `OUT_DIR` change; the work is already gated behind
  cargo's incremental rebuild mechanism via per-file
  `cargo:rerun-if-changed=` lines (`build.rs:113-115`).
- No memory allocation hot spot, no async, no GPU transfer/launch
  surface — none of the per-dimension audit prompts (pinned memory,
  stream lifetimes, fusion) apply to this crate.

## 2. Tests

- **No unit tests.** There is no `#[cfg(test)] mod tests` block in
  `src\lib.rs`.
- **No integration tests.** There is no `craton-gpu\tests\` directory
  at all (verified by directory listing).
- **No mock/fixture infrastructure.** The crate has no `dev-dependencies`
  block and no test-only modules.
- **Effective coverage: ~0%** of the build script and 0% of the
  16-line library. The library is two `env!()`-driven `const &str`
  declarations — there is no Rust behaviour to cover, but the build
  script's path-resolution and command-construction logic *is* testable
  and currently untested.

### Concrete additions (priority order)

1. **HIGH — `tests/build_script_resolves_env_var.rs`.** Compile-time
   integration test that sets `CRATON_GPU_JAVA_SRC` to a fixture
   directory containing a single `craton/gpu/Empty.java`, runs the
   build script in a child cargo invocation
   (`std::process::Command::new("cargo").arg("build")...`), and
   asserts the produced `ANNOTATIONS_DIR` is non-empty and contains
   `Empty.class`.
2. **HIGH — `tests/build_script_missing_java_src.rs`.** Same harness,
   `CRATON_GPU_JAVA_SRC` set to a non-existent path, asserts the build
   succeeds and emits a `cargo:warning=` containing "no .java source
   files found". Catches regressions of the resilient-fallback
   contract that the comment at `build.rs:28-30` promises.
3. **MED — `tests/build_script_missing_javac.rs`.** Override `PATH`
   to an empty directory (no javac), assert build succeeds with
   warning "javac not found". Confirms the no-JDK fallback.
4. **MED — `tests/links_metadata_format.rs`.** Spawn a tiny dummy
   downstream crate with `craton-gpu` as a build dep, and assert its
   build script sees `DEP_CRATON_GPU_ANNOTATIONS_ANNOTATIONS_DIR` and
   `_JAR` as env vars. This is the *contract* the entire crate exists
   to provide (see `Cargo.toml:17` + `build.rs:75-85`), and right now
   nothing pins it.
5. **LOW — proptest on `collect_java`.** Generate random directory
   trees and assert every emitted `PathBuf` ends in `.java` and points
   under `root`. Easy fuzz target.
6. **LOW — doctest on `ANNOTATIONS_JAR` / `ANNOTATIONS_DIR`** showing
   the env-var-lookup form a downstream `build.rs` should use. The
   README example at `README.md:29-34` is currently not compiled.

## 3. Documentation

### Existing

- `README.md` (46 lines) — accurate, scoped, calls out non-goals,
  shows the downstream `DEP_CRATON_GPU_ANNOTATIONS_*` pattern.
- `build.rs:1-31` — long module-level doc comment that explains the
  two-channel emission (rustc-env + cargo metadata) and *why* both
  are needed. Excellent context.
- `src\lib.rs:4-15` — crate-level doc + per-constant doc on both
  public items. 100% public-API rustdoc coverage.
- Workspace-level `docs\gpu\annotations.md` documents the *Java*
  side (the annotations themselves) thoroughly — Phase 1 reference,
  admission hints, examples, limitations.

### Missing

- **MED — pointer from `craton-gpu/README.md` to
  `docs/gpu/annotations.md`.** The two documents describe two halves
  of the same feature (the Rust packager vs. the Java surface) but
  do not link to each other.
- **MED — no documentation that the Java sources live in a *separate
  repository* (`craton-gpu-java`).** `build.rs:39-45` references it
  in code comments but the README does not mention the split at all.
  A first-time contributor cloning CratonVM will not understand why
  `ANNOTATIONS_JAR` is empty.
- **LOW — no `CHANGELOG.md` entry** scoped to craton-gpu. The
  workspace `CHANGELOG.md` exists; per-crate changelog would help
  given this crate's externalised source dependency.
- **LOW — no per-crate `NOTICE` or `LICENSE` file.** Apache-2.0
  permits relying on the workspace-root `NOTICE`/`LICENSE`; some
  publishing tooling expects them per-crate. With `publish = false`
  inherited (workspace `Cargo.toml:6`) this is moot today.
- **LOW — `links = "craton-gpu-annotations"` semantics** could use one
  more sentence in the README. The `Cargo.toml:13-17` comment is good
  but README readers may not know that no native library is actually
  linked.

## 4. OSS readiness

### Cargo.toml audit (`craton-gpu\Cargo.toml`)

- `name = "craton-gpu"` — fine; namespace not taken on crates.io as of
  the workspace `publish = false` posture.
- `version.workspace = true`, `edition.workspace = true`,
  `authors.workspace = true`, `license.workspace = true`,
  `repository.workspace = true` — all inherited cleanly. `Cargo.toml:3-7`.
- `keywords = ["gpu", "annotations", "java", "jvm"]`,
  `categories = ["development-tools::build-utils"]` — appropriate
  (`build-utils` correctly reflects what the crate is). `Cargo.toml:8-9`.
- `description` — concise and accurate. `Cargo.toml:10`.
- `readme = "README.md"` — present and points at a real file. `Cargo.toml:11`.
- `build = "build.rs"` — explicit and correct. `Cargo.toml:12`.
- `links = "craton-gpu-annotations"` — explained inline (`Cargo.toml:13-17`).
- **MISSING — `rust-version.workspace = true`.** Workspace pins MSRV
  to 1.77 (`Cargo.toml:10`), but craton-gpu's manifest does *not*
  inherit it. `jit-cuda\Cargo.toml:5` and `cuda-bridge\Cargo.toml:5`
  both do. Inconsistency, low risk.
- **MISSING — `[lints] workspace = true`.** Sibling crates set this
  (`jit-cuda\Cargo.toml:33-34`, `cuda-bridge\Cargo.toml:39-40`).
  Without it, craton-gpu does not inherit the workspace lint allowlist;
  no current symptom because there are no warnings anyway.
- **MISSING — `[lib]` is over-specified.** `path = "src/lib.rs"` at
  `Cargo.toml:19-20` is the cargo default; remove for tidiness.

### Headers / SPDX / NOTICE

- `src\lib.rs:1-2` carries the SPDX header
  `Apache-2.0 / Copyright 2024-2026 Craton Software Company`.
- `build.rs` has NO SPDX header. Add
  `// SPDX-License-Identifier: Apache-2.0` / `// Copyright 2024-2026 Craton Software Company`
  at the top of `build.rs` for consistency with `lib.rs`.
- `README.md:45` correctly states "Apache-2.0. See `LICENSE` and
  `NOTICE` at the workspace root." No per-crate `NOTICE` or `LICENSE`
  symlinks — acceptable for an Apache-2.0 workspace, but some
  downstream packagers (Debian, Fedora) want per-crate copies.

### NVIDIA-trademark surface

- Crate contains zero references to NVIDIA, CUDA, PTX, or any other
  third-party trademark. The workspace `TRADEMARKS.md` already
  covers them. Nothing to add.

### Blockers (publish or hand-off)

- **NONE.** With `publish = false` inherited at the workspace level,
  no immediate publish blockers exist.
- **If the workspace ever flips to `publish = true`**, the externalised
  Java source dependency (`build.rs:39-45`) is a hard blocker — a
  published crate cannot assume `C:/craton/craton-gpu-java/` exists.
  Options at that point: (a) re-vendor the `.java` files into
  `craton-gpu/src/main/java/` as the README still alleges they live
  (`README.md:12`), or (b) ship pre-compiled `.class` files in
  `src/main/resources/` and stop running `javac` at build time.

## Top 5 fix priorities

1. **MED — Decide on Java-source location and align code/docs.**
   `README.md:12` says the sources "ship under `src/main/java/`"; the
   actual `build.rs:39-45` says they live in a sibling repo or on
   `C:\craton\`. One of those statements is wrong. Either re-vendor
   the `.java` files into `craton-gpu/src/main/java/` (recommended,
   keeps the crate self-contained and unblocks future publish) or
   update the README to document the split, the env-var override,
   and the consequence that a default clone has empty annotations.
2. **MED — Add the four contract tests** (`build_script_resolves_env_var`,
   `build_script_missing_java_src`, `build_script_missing_javac`,
   `links_metadata_format`) listed under §2. Today the entire crate's
   reason to exist — exposing `DEP_CRATON_GPU_ANNOTATIONS_*` to
   `jit-cuda`'s build.rs — is unpinned.
3. **LOW — Add SPDX header to `build.rs`**, inherit
   `rust-version.workspace = true` and `[lints] workspace = true` in
   `Cargo.toml`, drop the redundant `[lib] path = "src/lib.rs"` line.
   All three together are <10 lines of churn.
4. **LOW — Cross-link `README.md` and `docs/gpu/annotations.md`.**
   The Rust side and the Java side of the same feature should
   reference each other.
5. **LOW — Replace `resolve_java_root()`'s "return the C-drive
   candidate even when missing" pattern with `Option<PathBuf>`** at
   `build.rs:173-190`. The current shape is correct but reads as
   accidental; an `Option` makes the missing-source case explicit.
