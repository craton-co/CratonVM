# vm-cli review

Crate: `C:\Projects\CratonVM\vm-cli\` — `cratonvm-cli` v0.3.0, Apache-2.0, MSRV 1.77,
edition 2021, `publish = false` (inherited).

Files reviewed (exhaustive):
- `Cargo.toml` (77 LOC) — two `[[bin]]` targets (`cratonvm`, `java`), 5 features
  (`mimalloc` default, `synthetic-jdk`, `gpu`, `gpu-driver`), dev-deps `tempfile`/`zip`.
- `README.md` (44 LOC) — crate-local readme with scope/non-goals/usage.
- `src/main.rs` (3159 LOC) — single-file launcher; 52 unit tests at the tail.
- `tests/common/mod.rs` (78 LOC) — test helpers (`stage_class`, `cratonvm_cmd`,
  `build_jar`).
- `tests/cli_helloworld.rs`, `cli_classpath_dir_vs_jar.rs`, `cli_main_args.rs`,
  `cli_missing_class.rs`, `cli_nojit.rs`, `cli_uncaught_exception.rs`,
  `cli_xmx_compat.rs`, `no_diag_eprintln.rs` — 7 cli_*.rs + 1 workspace-CI gate
  test = 14 `#[test]` (3 `#[ignore]`d).
- `tests/resources/{HelloWorld,PrintArgs,Thrower}.{java,class}` — committed
  fixtures.

## Summary

- **HIGH** — `cli_nojit.rs::nojit_cli_flag_currently_absent` (lines 64-97) and
  `cli_xmx_compat.rs::xmx_single_dash_currently_rejected_by_clap` (lines 98-115)
  are stale: `--nojit` is now in clap (`src/main.rs:77-78`) and `-Xmx256m` is
  rewritten by `normalize_java_launcher_argv` (`src/main.rs:579-588`). These
  "regression-marker" tests now `panic!()` whenever they run — they break CI on
  the very day the fix lands and were not removed.
- **HIGH** — `publish = false` (workspace `Cargo.toml:6`) + path-only deps to
  `cratonvm-vm`, `cratonvm-native-api`, `cratonvm-native-builtins`,
  `cuda-bridge` block any `cargo install cratonvm-cli` flow.
- **HIGH** — `[[bin]] name = "java"` (`Cargo.toml:19-21`) shadows the system
  `java` on `cargo install`; no opt-in feature gate. Real safety issue for end
  users with a working JDK on PATH.
- **MED** — `string_array_class_id` lookup at `src/main.rs:1666-1669` is now
  correct (`load_class_concurrent("java/lang/String")` with `ClassId::new(0)`
  only as fallback), but several MED bugs from prior review remain:
  manifest `Class-Path` traversal, world-readable temp JAR staging, watchdog
  has no main-finished cancellation flag, `CRATONVM_DEFAULT_WATCHDOG_SEC=0`
  immediately aborts.
- **MED** — Docs drift: `README.md:5`/`docs/INSTALL.md:37`/`BUILD_GUIDE.md:22`
  state Rust 1.75+ but workspace `Cargo.toml:10` pins MSRV 1.77.
  `docs/INSTALL.md:81-82,87` still shows `--synthetic-jdk=true/false` (clap
  parses it as a bool flag, no value).

## 1. Code review

### Bugs

| Sev | Location | Issue |
| --- | --- | --- |
| **MED** | `src/main.rs:874-1042` (JAR staging) | The non-`.jar` archive staging copies bytes to a predictable `cratonvm-<pid>-<ms>-<stem>.jar` in `std::env::temp_dir()`. `stem` is derived from user-supplied `--jar` via `file_stem()` (`src/main.rs:973-976`) — characters other than `/`/`\` flow through. `create_new` mitigates symlink-clobber, but the file is opened without `.read(true)` or any permission restriction; on Unix the umask leaves it world-readable. No cleanup on exit (`src/main.rs:967-972` acknowledges this). For confidential application JARs (signed apps, license-protected code), `/tmp` leaks bytes for the process lifetime. Recommendation: `tempfile::NamedTempFile` (already a dev-dep, easy to promote) or explicit `chmod 0600` after open. |
| **MED** | `src/main.rs:949` | `ClassPath::read_jar_manifest(jar_path)` then `manifest.resolve_class_path(jar_path)` (`src/main.rs:1048`) — manifest `Class-Path:` entries are taken at face value. A malicious JAR with `Class-Path: ../../etc/secret.jar` resolves to an arbitrary parent directory. HotSpot has the same exposure ("trust the JAR provider"), but unlike HotSpot we don't even emit a warning. Add a check that resolved paths stay within `jar_path.parent()` or under known-safe roots, or document the threat surface in `SECURITY.md`. |
| **MED** | `src/main.rs:1057-1090` (Quarkus app-dir scan) | `--jar` mode unconditionally walks `app/`, `quarkus/`, `lib/main`, `lib/boot`, `lib/deployment` relative to **both** the jar dir and its parent, slurping every `*.jar`. No cap on count, no de-duplication against the existing `cp`, no symlink-loop guard (`canonicalize` is called but a malicious symlink farm could still inflate the list). Should at least de-dupe and cap (e.g. 4096 entries). |
| **MED** | `src/main.rs:1380-1404` (watchdog cancellation) | The watchdog has no "main thread completed" cancellation. After 120s (default) it unconditionally calls `process::abort()` (`src/main.rs:1550`). A normal exit happening at T-1ms still races the abort and exits via `SIGABRT`/0xC0000409. Add an `Arc<AtomicBool>` `main_finished` flag the watchdog checks immediately before abort. |
| **MED** | `src/main.rs:1384-1404` (`CRATONVM_DEFAULT_WATCHDOG_SEC=0`) | The CLI flag treats `--stack-dump-on-timeout=0` as "disabled" (`src/main.rs:1382`), but the env-var path uses `.unwrap_or(120)` (`src/main.rs:1400`) — a value of `0` is *valid* `.parse()` output, so `CRATONVM_DEFAULT_WATCHDOG_SEC=0` yields `Some(0)` and aborts immediately. Inconsistent with the flag contract. Fix: `…unwrap_or(120).max(1)` or treat 0 as disabled both ways. |
| **MED** | `src/main.rs:1191-1203, 1218-1231, 1119-1131` | `--Xshare` and `--XX:AOTMode` `default_value = "off"` with warn-and-default on unknown values. `--Xverify` warns and clears the override (preserving `--noverify`). HotSpot exits non-zero on unknown enum values. Recommend `bail!` for unparseable mode strings so typos surface loudly. |
| **MED** | `src/main.rs:1106-1112` | When no `--classpath` is given, `std::env::var("CLASSPATH")` is honoured silently. There is no validation that env-CP entries exist or are sane. HotSpot semantics, but a security-conscious launcher should warn ("classpath sourced from environment"). |
| **MED** | `src/main.rs:1894-2284` (cause-chain renderer field-index walk) | The Throwable-field walk (`walk = Some(cid); while let Some(k) = walk { … walk = cls.superclass }`) iterates root→derived order, picking the FIRST `detailMessage`/`cause`/`stackTrace` match. For Throwable subclasses that shadow the field, the superclass slot wins. HotSpot resolves the most-derived declaration. Reverse the walk or pick the last match. Same pattern at `src/main.rs:2131-2151`, `2170-2199`, `2236-2255`. |
| **MED** | `src/main.rs:249-284` (`validate_class_name`) | ASCII-only validator (`is_ascii_alphabetic`/`is_ascii_alphanumeric`) rejects Unicode identifiers — JVMS §4.2.2 permits any Java identifier, including `is_alphabetic()` code points. Legitimate non-ASCII class names will be rejected. Either accept Unicode `is_alphabetic` or document the deliberate restriction in the error message. |
| **LOW** | `src/main.rs:387-427` (`VALUE_TAKING_OPTS`) | Hard-coded constant list, must stay in sync with every clap `#[arg(...)]` that takes a value. Easy to miss when adding a new flag. Add a debug-time assertion that iterates the clap matches schema and verifies every value-taking option is listed, or derive the list from a single source. |
| **LOW** | `src/main.rs:308` (`AGGREGATES`) | Hard-coded to a single entry `("netty-all.jar", "netty-")`. The function name `expand_aggregate_jars` implies general support; either delete the helper and inline the Netty case, or add documented `groovy-all.jar`, `jackson-all.jar`, etc. so the abstraction earns its keep. |
| **LOW** | `src/main.rs:993-1024` | The 16-attempt retry loop for staging unique temp files swallows all but the LAST `io::Error`. When 16 attempts fail, the user sees only `"could not create a unique temp file in {dir}"` without the underlying `EPERM`/`ENOSPC`/`EBUSY` cause. Wrap with the last error in the message. |
| **LOW** | `src/main.rs:2537-2559` (`parse_size`) | Recognises `k`/`m`/`g` but not `t` (terabyte). Modern very-large-heap installs use `-Xmx16t`. Add `b't' \| b'T'` arm with `checked_mul` for overflow safety. |
| **LOW** | `src/main.rs:16` | `static GLOBAL: mimalloc::MiMalloc` — convention for `#[global_allocator]` statics is `ALLOC`/`ALLOCATOR`. Pure style. |
| **LOW** | `src/main.rs:2390-2534` (panic hook + main) | The panic hook routes everything to direct stderr; the dispatch trace ring is enabled only when the watchdog is on. A panic from the JIT/interpreter during `main()` therefore prints the Rust message but no dispatch context (which Java method was active). Consider enabling the ring unconditionally when `RUST_BACKTRACE` is set. |
| **LOW** | `src/main.rs:1648-1679` (main args + String[] alloc) | The String[] array is allocated AFTER `vm.load_class(&class_name)` (line 1640), so a startup-cost-sensitive embedder cannot pre-allocate. Negligible for normal use; flagged because the file is "startup-time-critical" per scope. |
| **LOW** | `src/main.rs:46` (`#[arg(short = 'c', long = "classpath", alias = "cp")]`) | Short `-c` for classpath is non-standard for `java`-launcher (HotSpot uses `-cp`/`-classpath` only). On the `java` bin alias this may surprise users. The `alias = "cp"` is the HotSpot-compatible form; consider dropping `-c`. |

### Vulnerabilities

| Sev | Location | Issue |
| --- | --- | --- |
| **MED** | `src/main.rs:1106-1112` | Classpath injection via `CLASSPATH` env var (see MED above). Matches HotSpot semantics; document the threat. |
| **MED** | `src/main.rs:711-747` (`extract_system_properties`) | `-D<key>=<value>` flows into `config.system_properties` unfiltered. An attacker who controls argv can override `java.security.policy`, `jdk.serialFilter`, `java.system.class.loader`, etc. Matches HotSpot; document. |
| **MED** | `src/main.rs:774-808` (`extract_hotspot_flags`) | `-agentlib:`, `-agentpath:`, `-javaagent:` accepted without filtering — `dlopen` of attacker-controlled `.so`/`.dll`. Matches HotSpot; document in `SECURITY.md`. |
| **MED** | `src/main.rs:1048` (manifest Class-Path traversal) | See "MED — manifest Class-Path traversal" above. |
| **MED** | `src/main.rs:991-1042` (temp JAR staging permissions) | See "MED — JAR staging" above. World-readable on Unix. |

### Stubs / unimplemented / todo / FIXME

- `grep -E 'TODO\|FIXME\|todo!\|unimplemented!\|panic!\('` over `src/` and `tests/`
  → **two comment-only follow-ups**, **zero `todo!`/`unimplemented!`/runtime
  stubs in `run()`**:
  - `src/main.rs:1877-1878` — `T2.2.18` roadmap note about populating
    `Throwable.stackTrace[]` synthetically (see `docs/roadmap-100.md` line 471).
  - `src/main.rs:1873-1876` — same area; comment about lazy-write of
    `stackTrace` field.
- Three `unwrap()`s outside tests:
  - `src/main.rs:935` `args.class_name.take().unwrap()` — guarded by the
    `is_some() && is_some()` check at line 933.
  - `src/main.rs:1105` `args.class_name.as_ref().unwrap()` — guarded by line
    927's `is_none() && is_none()` early-bail.
  - `src/main.rs:2529-2533` `builder.spawn(...).expect("failed to spawn main-vm
    thread")` and `handler.join().unwrap_or_else(...)` — these are infallible in
    practice but a thread-spawn failure surfaces as a panic message; acceptable.
- 1 `panic!("main() panicked: …")` at `src/main.rs:1716` — that is intentional
  bail-to-message conversion inside `catch_unwind`.

### Performance

| Sev | Location | Issue |
| --- | --- | --- |
| **LOW** | `src/main.rs:1894-2284` (renderer) | Cause-chain renderer takes `class_manager.read()` inside an 8-deep loop, recomputing Throwable field indices each iteration. The "PERF" comments at 1888-1893 / 2162-2165 / 2226-2229 hoist some reads, but field-index resolution is repeated. Cache `(ClassId → (msg_i, cause_i, stack_i, target_i))` once per process. Only matters for deep cause chains (Spring/JNDI). |
| **LOW** | `src/main.rs:847-853` (argv pre-processing) | Four passes over `Vec<String>` (each clones the strings: `insert_program_args_separator` → `normalize_java_launcher_argv` → `extract_system_properties` → `extract_hotspot_flags`). For typical 5-10 args fine; for argv with a 10k-entry `-cp` string and many flags this is measurable. Could fuse into a single state machine. |
| **LOW** | `src/main.rs:1058-1060` (`std::fs::canonicalize(jar_path)`) | Called once per `--jar` run. Stat syscall; negligible. |
| **LOW** | `src/main.rs:1414-1420` (`native_ring::enable` + `dispatch_trace::enable`) | Always armed when a watchdog is enabled — including the 120s default watchdog. Recording cost is documented as a single relaxed atomic load per entry, but every Java method/native call now pays it on every run. Consider arming lazily (only when watchdog actually fires). |
| **INFO** | `Cargo.toml:36-42` | `mimalloc` global allocator is a real Windows win (documented). |

## 2. Tests

### Inventory

- **Unit tests** (`src/main.rs:2562-3158`): **52** `#[test]` covering 8 helper
  groups:
  - `insert_program_args_separator` — 7 tests.
  - `parse_size` — 4 tests.
  - `validate_class_name` — 10 tests.
  - `extract_system_properties` — 3 tests.
  - `normalize_java_launcher_argv` — 1 test + 11 HotSpot-rewrite tests at
    `src/main.rs:2977-3119`.
  - `extract_hotspot_flags` — 5 tests.
  - `expand_aggregate_jars` — 4 tests with `unique_temp_dir` helper.
  - clap end-to-end smoke — `hotspot_xmx_passes_clap_after_full_pipeline`
    (`src/main.rs:3137`), `nojit_flag_is_accepted_by_clap`
    (`src/main.rs:3153`).
- **Integration tests** (`tests/cli_*.rs`): **7** files, **13** `#[test]`,
  including 3 `#[ignore]`d (`cli_xmx_compat.rs`). Plus 1 workspace-CI gate
  (`no_diag_eprintln.rs`). All e2e spawn the binary via `env!("CARGO_BIN_EXE_cratonvm")`.

### Coverage — improved, still BELOW 85%

The unit tests cover the **pure-function helpers** thoroughly (~400 LOC / 3159 =
~13% of file by lines, but ~95%+ branch coverage on those helpers). The
integration tests now exercise a small surface end-to-end: HelloWorld via dir
+ jar classpath, missing-class error, uncaught-exception render,
PrintArgs argv plumbing, `CRATONVM_DISABLE_JIT=1` env path.

The major orchestration surface is still mostly untested:
- `run()` (`src/main.rs:810-2388`, ~1580 LOC) — JAR mode classpath construction
  (945-1101), Quarkus app-dir scan (1057-1090), watchdog spawn (1380-1556),
  `initPhase1` driver (1572-1622), java-agent dispatch (1682-1696), exception
  cause-chain renderer (1860-2384), panic hook (2424-2516).

Best-case line coverage estimate: **~40-50%** (still under 85%). The e2e tests
do exercise much of `run()` indirectly (every cli_*.rs invokes it), but no
test asserts internal state — only stdout/stderr/exit-code.

### Stale tests (require action this round)

| File:line | Problem |
| --- | --- |
| `tests/cli_nojit.rs:64-97` | `nojit_cli_flag_currently_absent` asserts clap REJECTS `--nojit`. But `--nojit` is now a real flag (`src/main.rs:77-78`, set-var at 867-869). This test will fail on every CI run. The file's header comment at lines 4-7 explicitly says "There is currently NO `--nojit` CLI flag" — also stale. **Delete this test and update the doc-comment; or flip the assertion to `status.success()`.** |
| `tests/cli_xmx_compat.rs:57-91` | Two tests `#[ignore]`d "until pre-clap rewriter lands". The rewriter HAS landed (`src/main.rs:579-588`). **Remove `#[ignore]` and rely on the existing assertions.** |
| `tests/cli_xmx_compat.rs:98-115` | `xmx_single_dash_currently_rejected_by_clap` panics IF the fix is in place. With the rewriter in `normalize_java_launcher_argv` shipped, this test panics on every run. **Delete it.** |

### Gaps (additions to push toward 85%)

| Surface | Suggested test |
| --- | --- |
| `--help` / `--version` smoke | `tests/cli_help.rs` — `cratonvm --help` exit 0, includes "cratonvm" + flag names; `cratonvm --version` matches `env!("CARGO_PKG_VERSION")`. |
| `--java-home /nonexistent` rejection | The validator at `src/main.rs:1171-1178` bails; no test asserts the error message. |
| `--Xmx` overflow + invalid size error path | Currently only `parse_size` unit-tests cover this. Add e2e: `cratonvm --Xmx 9999999999g HelloWorld` exit 1 with "Invalid heap size". |
| Watchdog `effective_watchdog` 3-axis matrix | Refactor `effective_watchdog` to a pure helper `fn(cli: Option<u64>, env_disable: Option<String>, env_default: Option<String>) -> Option<u64>` and table-test (CLI=Some(0), env_disable=Some("1"), parse failures, value=0 from env, etc.). |
| `--jar foo.war` (non-.jar staging path) | `tests/jar_war_staging.rs` — synthesize a minimal `.war` with a `Main-Class` manifest, invoke, assert the staged `cratonvm-*.jar` appears in `std::env::temp_dir()` AND the run succeeds. Exercises the entire `src/main.rs:965-1042` JN3 block. |
| Manifest `Class-Path` traversal | Build a JAR with `Class-Path: ../../etc/secret.jar` and assert it does NOT resolve outside `jar_path.parent()` (after the fix lands). |
| Exit-code matrix | `tests/cli_exit_codes.rs` — assert exit codes for: clean run (0), missing class (1), uncaught exception (1), no args (1), clap parse error (2). HotSpot uses `1` for unhandled exception, `2` for clap-style errors. |
| `-D` properties past `--` separator | `tests/cli_d_props.rs` — `cratonvm -Da=b -- -Dc=d Main` ⇒ `a=b` is set, `-Dc=d` reaches the program. Pure-function test for `extract_system_properties` covers the unit path; e2e is missing. |
| `-XX:+HeapDumpOnOutOfMemoryError` end-to-end | Unit test covers extraction; no e2e asserts it lands in `VmConfig.heap_dump_on_oom`. |
| `--add-exports module/pkg=target` malformed | `src/main.rs:1273-1284` warns-and-continues on a bad spec. Assert the warning text. |
| GPU flag matrix (cfg-gated) | When `--features gpu` is on, `cratonvm --gpu-info` must exit 0 (or print "no CUDA device"). No test today. |

### Brittle / risky test patterns

- `tests/common/mod.rs:32-42` (`stage_class`): panics on copy failure. Acceptable
  for a test helper.
- `tests/cli_helloworld.rs` (and siblings) use `child.wait()` after manual pipe
  reads — no timeout, so a deadlock in the launcher hangs CI. Use
  `child.wait_timeout()` from a small helper (the `tempfile`+`zip` dev-dep set
  is fine; adding `wait-timeout` is one more line).
- `src/main.rs:2904-2912` (`unique_temp_dir`) — uses `std::env::temp_dir()` +
  a process-static counter; the directory is never cleaned up on test failure
  (the `let _ = std::fs::remove_dir_all(dir);` cleanups only run on success).
  Swap to `tempfile::tempdir()`.

## 3. Documentation

### Existing (inventory)

- `vm-cli/README.md` (44 LOC) — crate-local scope/non-goals/usage/license.
  ✓ Mentions both `cratonvm` and `java` bin names. ✓ Apache-2.0 + copyright
  footer.
- `src/main.rs` — extensive inline rustdoc on every `#[arg(...)]` field (29
  documented flags + 4 GPU flags) and every top-level fn. The argv-pipeline
  ordering rationale (`src/main.rs:840-853`) is well-explained. The exception
  renderer's PropertyBatchUpdateException special case (~166 LOC at
  `src/main.rs:2129-2295`) is over-documented but useful.
- SPDX header present at `src/main.rs:1-2` and `tests/no_diag_eprintln.rs:1-2`
  ✓ — the other test files lack headers (see OSS readiness).

### Missing / drift

| Item | Status | Action |
| --- | --- | --- |
| Crate-level `#![doc = ...]` rustdoc | **Missing** — `src/main.rs` has no module-level summary block before the first `use`. README.md exists but `cargo doc -p cratonvm-cli` produces an empty top page. | Add a `//!` block summarising scope + linking to flags. |
| `--help` snapshot | **Missing** — no `docs/cli-help.txt` checked in. | `cratonvm --help > docs/cli-help.txt`; add a doc-test that diffs against the live output. |
| Man page | **Missing** — no `cratonvm.1` or `docs/cratonvm.1.md` (ronn/pandoc-friendly). | Conventional for a `java`-equivalent CLI; useful for `apt`/`brew` packaging. |
| MSRV mismatch | `BUILD_GUIDE.md:22` "1.75+", `docs/INSTALL.md:37` "Rust 1.75+", `README.md:5` "rust-1.75%2B" — but workspace `Cargo.toml:10` pins `1.77`. | Bump every reference to 1.77 OR lower workspace MSRV to 1.75. |
| `docs/INSTALL.md:81-82` `--synthetic-jdk=true / =false` | **WRONG.** `src/main.rs:97-98` declares `--synthetic-jdk` as a boolean flag (no value). `--synthetic-jdk=true` will likely error or set the flag with an extraneous arg. | Replace with bare `--synthetic-jdk` and explain default depends on JAVA_HOME (`src/main.rs:1207-1216`). |
| `docs/INSTALL.md:87` example | Same issue — uses `--synthetic-jdk=false --java-home …`. The correct invocation is just `--java-home …` (auto-detects real JDK). | Replace example. |
| `BUILD_GUIDE.md:45` "Output `target/release/cratonvm`" | Two binaries are produced (`cratonvm.exe` + `java.exe` per `Cargo.toml:13-21`). | Add a note about the `java` bin alias and why it exists. |
| README.md (root) flag table consistency | `README.md:74-81` table lists `--Xmx`, `--Xbootclasspath`, `--java-home`, `--nojit`, `--noverify`. ✓ Matches current code now (after `--nojit` landed). | None — confirmed consistent. |
| Env-var docs | `CRATONVM_DBG_EXIT` (line 832), `CRATONVM_INTRINSIC_STATS` (line 1766), `CRATONVM_DBG_CHARSET` / `CRATONVM_DBG_ATHROW` (line 2377-2379), `CRATONVM_DEFAULT_WATCHDOG_SEC` (line 1390), `CRATONVM_DISABLE_DEFAULT_WATCHDOG` (line 1384), `CRATONVM_DISABLE_JIT` (line 868), `CRATONVM_STRICT_SWALLOWS` (line 1788). | Not all are in `docs/CONFIG.md` (referenced by prior review); audit and add the missing ones to a single env-var reference. |
| `cli_nojit.rs` doc comment lines 4-7 | Stale ("There is currently NO `--nojit` CLI flag"). | Delete; flag now exists. |
| `cli_xmx_compat.rs` doc comment lines 1-19 | Stale (asserts the fix is missing). | Delete or rewrite. |
| `vm-cli/Cargo.toml:17-21` comment | "Maven Surefire's `<jvm>`/`-Djvm=` validation" — still accurate. ✓ | None. |

### Consistency with BUILD_GUIDE.md / docs/INSTALL.md

- BUILD_GUIDE.md uses `cargo run --release -p cratonvm-cli -- …` throughout —
  matches the crate name. ✓
- BUILD_GUIDE.md:135-161 directory tree shows `vm-cli/` with comment "CLI
  entry point". ✓
- INSTALL.md:42 `cargo build --release -p cratonvm-cli` — ✓.
- INSTALL.md:75-87 "JDK Mode" section is the main drift point (see above).
- Neither doc mentions `--jdwp-port` / `--jdwp-suspend` / agent options /
  `-D` system properties / `--module-path` / `--add-exports` etc. — only the
  6-flag subset. Consider linking to `docs/CONFIG.md` from both.

## 4. OSS readiness

### Cargo.toml metadata

| Field | Value | Verdict |
| --- | --- | --- |
| `name` | `cratonvm-cli` | ✓ unique, hyphenated |
| `version` | `0.3.0` (workspace) | ✓ |
| `edition` | `2021` | ✓ |
| `rust-version` | `1.77` (workspace) | ✓ but doc drift (see §3) |
| `license` | `Apache-2.0` (SPDX) | ✓ |
| `repository` | `https://github.com/craton-co/cratonvm` | ✓ |
| `keywords` | 5 entries, all valid | ✓ |
| `categories` | `compilers`, `emulators` | ✓ |
| `description` | "Command-line launcher for the CratonVM" | ✓ |
| `readme` | `"README.md"` (now points inside crate) | ✓ — improved since prior review. |
| `homepage` | inherited (workspace) | ✓ |
| `documentation` | **Missing** | Recommend `documentation = "https://docs.rs/cratonvm-cli"`. |
| `authors` | `["Craton Software Company"]` (workspace) | ✓ |
| **`publish`** | inherits `publish = false` from `Cargo.toml:6` | **BLOCKER** — must add `publish = true` override here, or remove the workspace setting. |
| `[[bin]] name = "java"` | exists | **HIGH risk on `cargo install`** — shadows system `java`. Recommend feature-gate (e.g. `surefire-compat = []` + `#[cfg(feature = "surefire-compat")]` on the second `[[bin]]`) OR loud documentation in README. |

### SPDX headers

- `src/main.rs:1-2` ✓ `SPDX-License-Identifier: Apache-2.0` + copyright.
- `tests/no_diag_eprintln.rs:1-2` ✓.
- `tests/common/mod.rs`, `tests/cli_*.rs` (7 files) — **no SPDX header**. Per
  Apache 2.0 §4(c), source files should carry attribution. Add headers
  consistently across all `.rs` files in the crate.

### Binary distribution / crates.io blockers

1. **`publish = false`** at workspace root must be lifted OR per-crate
   `publish = true` override added.
2. **Path-only deps** — `cratonvm-vm`, `cratonvm-native-api`,
   `cratonvm-native-builtins` (all path deps in `Cargo.toml:24-30`), and
   optional `cuda-bridge` (lines 47-49). crates.io rejects path-only deps. All
   must be published first, then declared as `version = "0.3"` deps.
3. **Binary name reservation on crates.io** — both `cratonvm-cli` (crate
   name) and the `cratonvm`/`java` binary names are not yet reserved.
   Recommend reserving early via a stub release.
4. **`java` bin** — once published, `cargo install cratonvm-cli` will install
   `~/.cargo/bin/java` and shadow real JDK. Either:
   - feature-gate the `[[bin]] name = "java"` block behind a non-default
     `surefire-compat` feature, OR
   - rename the second binary to something like `cratonvm-java`, OR
   - document loudly in README + INSTALL that `cargo install` will shadow
     `java` (and provide an `--no-default-features` install path).
5. **Pre-built binary distribution** — `docs/INSTALL.md:5-11` references
   `cratonvm-x86_64-linux-gnu.tar.gz` etc. on GitHub Releases. The release
   pipeline is not in this crate; verify `ci/` / `.github/workflows/` exists
   and builds both binaries.

### Other OSS items

- `LICENSE`, `NOTICE`, `AUTHORS`, `SECURITY.md`, `CODE_OF_CONDUCT.md`,
  `CONTRIBUTING.md`, `CHANGELOG.md` all exist at workspace root ✓.
- `SECURITY.md` should be reviewed for whether it covers vm-cli-specific
  attack surfaces flagged under §1 Vulnerabilities (CLASSPATH env, manifest
  Class-Path traversal, agentlib `dlopen`, world-readable temp JAR).
- `CHANGELOG.md` (workspace) — verify `--nojit`, HotSpot single-dash compat,
  and JN3 non-.jar staging are mentioned per release.

### Summary verdict

**Not ready for crates.io publication today.** Required before publishing
`cratonvm-cli`:
1. Resolve workspace `publish = false`.
2. Publish path deps (`cratonvm-vm`, `cratonvm-native-api`,
   `cratonvm-native-builtins`, `cuda-bridge`) as crates first; switch to
   version deps.
3. Decide and document the `[[bin]] name = "java"` strategy (feature-gate
   recommended) — otherwise `cargo install` shadows system `java`.
4. Delete/repair stale tests in `cli_nojit.rs` and `cli_xmx_compat.rs`.
5. Fix `docs/INSTALL.md` `--synthetic-jdk=true/false` syntax.
6. Bump MSRV references to 1.77 across `README.md`, `docs/INSTALL.md`,
   `BUILD_GUIDE.md`.
7. Add SPDX headers to remaining `tests/*.rs`.

For GitHub-only OSS release (no crates.io): items 3-7 are blockers; items 1-2
are deferred.

## Top 5 fix priorities

1. **Repair stale tests** (`tests/cli_nojit.rs:64-97`,
   `tests/cli_xmx_compat.rs:57-115`). They now `panic!()` because the
   features they claim are missing have shipped. Either delete them or flip
   the assertions to assert the success path. This breaks CI every run and is
   the highest-impact, lowest-effort fix.
2. **Resolve `[[bin]] name = "java"` collision** (`Cargo.toml:19-21`).
   Feature-gate behind `surefire-compat = []` so `cargo install` does NOT
   shadow system `java` by default. Hard blocker for crates.io distribution.
3. **Workspace `publish = false` + path deps**
   (`Cargo.toml:6` + `vm-cli/Cargo.toml:24-30,47-49`). Block crates.io
   publication entirely. Plan a crates.io rollout (`cratonvm-types` →
   `cratonvm-reader` → … → `cratonvm-cli`).
4. **MED security bundle** — (a) `tempfile::NamedTempFile` for non-.jar
   staging (`src/main.rs:991-1042`) with explicit `chmod 0600`; (b) manifest
   Class-Path traversal check (`src/main.rs:1048`); (c) Quarkus app-dir scan
   de-dupe + cap (`src/main.rs:1057-1090`); (d) watchdog "main finished"
   cancellation flag (`src/main.rs:1380-1404`); (e) `CRATONVM_DEFAULT_WATCHDOG_SEC=0`
   consistency fix.
5. **Doc drift** — (a) `docs/INSTALL.md:81-87` `--synthetic-jdk=true/false`
   → bare flag; (b) MSRV 1.75 → 1.77 across `README.md:5`,
   `docs/INSTALL.md:37`, `BUILD_GUIDE.md:22`; (c) add a crate-level `//!`
   rustdoc block to `src/main.rs` so `docs.rs/cratonvm-cli` is not empty;
   (d) snapshot `cratonvm --help` into `docs/cli-help.txt` for drift
   detection.
