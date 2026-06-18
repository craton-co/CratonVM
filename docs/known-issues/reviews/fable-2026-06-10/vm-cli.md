# Code Review — `vm-cli` crate (CratonVM)

Reviewer: Fable (Opus 4.8) — 2026-06-10
Scope: `vm-cli/src/main.rs` (~3.6k LOC, single file) and `vm-cli/tests/` (9 files + `common/mod.rs`).
Method: static review only (no cargo build/test/clippy run). Whole-file read of `main.rs`
(in chunks) and all test files.

---

## Summary

`vm-cli` is the `java`-equivalent launcher: it normalizes a HotSpot-style argv into a
clap-parseable form, extracts `-D` properties and `-XX`/`-agent` flags, builds a `VmConfig`,
boots `System.initPhase1`, invokes `main(String[])`, and renders uncaught-exception stack
traces. The code is careful and heavily commented; security posture is good (no shell-outs, no
command injection, temp-file staging uses `create_new` exclusive-create to close an
arbitrary-write vector, `parse_size` uses `checked_mul`). There are **no memory-safety issues**:
the single `unsafe` block is a test-only deliberate SEGV gated behind `CRATONVM_TEST_SEGV=1`.

The real weaknesses are (1) **argv-normalization correctness** — the multi-stage pre-clap
pipeline has an order-of-operations bug for the separate-token `-Xms <size>` form that the code
explicitly claims to support, plus `-X` single-dash flag gaps; (2) **launcher fidelity** — the
blanket `--` strip drops a `--` that a Java program legitimately wants; and (3) **CLI/env-var
sprawl** — 13 distinct `CRATONVM_*` env gates are read in this one file with no `--help`
discoverability, several duplicating CLI flags.

No forbidden synthetic "fake main" stubs exist in this crate (correctly — that policy targets
`native-builtins/*_extras.rs`). The closest things to stubs here are the GPU `--gpu-info`/`--gpu`
probe paths, which honestly report "no CUDA device" rather than faking success.

---

## Bugs

### B1 (HIGH) — `-Xms <size>` separate-token form is mishandled, breaking the argv pipeline
`vm-cli/src/main.rs:808` (`normalize_java_launcher_argv`)

`VALUE_TAKING_OPTS` lists `-Xms` (line 504) specifically so `insert_program_args_separator`
treats `-Xms 512m` as an option+value pair. But `normalize_java_launcher_argv` handles `-Xms`
with a prefix branch that drops only the `-Xms` token and leaves the value standing:

```rust
else if a.starts_with("-Xms") {
    ... i += 1;   // consumes ONLY "-Xms"; "512m" is left in the stream
}
```

For `java -Xms 512m Main`:
- stage 1 (`insert_program_args_separator`) consumes `-Xms`+`512m` as a pair, then inserts `--`
  after `Main`, yielding `[java, -Xms, 512m, Main, --, ...]`.
- stage 2 (`normalize`) drops `-Xms`, leaving `[java, 512m, Main, --]`.
- clap then sees `512m` as the bare main-class positional and `Main` as a stray second
  positional → either an "unexpected argument" error or it runs the wrong "class" `512m`.

The inline form `-Xms512m` works (the dropped token carries the value), which is why the inline
case passes and this is untested. Maven Surefire is the documented motivation for accepting
`-Xms` at all, and Surefire/Gradle forks commonly emit the separate-token form. Fix: in the
`-Xms` branch, if `a == "-Xms"` consume the following token too (`i += 2`), mirroring the
`-Xmx`/`-Xshare`/`-Xverify` separate-token branches.

### B2 (MEDIUM) — Blanket `--` strip drops a separator the Java program legitimately receives
`vm-cli/src/main.rs:1067`

```rust
args.args.retain(|a| a != "--");
```

This removes **every** `--` from the program-args vector, not just the single launcher-inserted
separator. Stock `java Main a -- b` must deliver `["a", "--", "b"]` to `main` (many CLI programs,
e.g. those using their own `--` end-of-options convention, depend on this). Here the user's `--`
is silently dropped, delivering `["a", "b"]`. Fix: remove at most one `--`, and only the one the
launcher inserted (track its index from `insert_program_args_separator` rather than filtering by
value), or strip only the first occurrence.

### B3 (MEDIUM) — Other `-X` single-dash flags are rejected, breaking drop-in `java` compat
`vm-cli/src/main.rs:827` (final `else` of `normalize_java_launcher_argv`)

`-XX:`-prefixed flags are broadly swallowed (lines 814-826), but single-`X` flags other than the
handful explicitly handled (`-Xmx/-Xms/-Xshare/-Xverify/-Xbootclasspath/-Xlog/-noverify`) fall
through to the final `else`, get pushed verbatim, and clap rejects them as "unexpected argument".
Common HotSpot flags that hit this: `-Xss<size>` (thread stack size — Surefire/Gradle pass this
routinely), `-Xint`, `-Xbatch`, `-Xrs`, `-XshowSettings`, `-Xnoclassgc`. A drop-in `java` should
accept-and-ignore unknown `-X` knobs the same way it does `-XX:`. Add a catch-all
`else if a.starts_with("-X")` arm that ignores (with the same `CRATONVM_DBG_ARGS` trace) instead
of letting them reach clap.

### B4 (LOW) — `-Xms` with no value as the last token leaves the pipeline in a bad state
`vm-cli/src/main.rs:587` + `:808`

Related to B1: if `-Xms` (no inline value) is the final token, `insert_program_args_separator`
does `i += 1` (line 593) without inserting `--`, and `normalize` drops it; harmless here but
indicates the `-Xms` handling is not symmetric with the other value-taking options. Low because
a bare trailing `-Xms` is not a realistic invocation.

### B5 (LOW) — Inconsistent dotted-vs-slash class name in the load-failure message
`vm-cli/src/main.rs:1886-1890`

`bail!("Could not find or load main class {}: {e}", class_name.replace('/', "."))` dot-normalizes
the name in the message, but `e` (the inner error from `load_class`) typically embeds the
slash-form name (`class not found: DoesNotExist` vs a fully-qualified path), so the same class is
shown two ways for packaged classes. Cosmetic; the existing `cli_missing_class.rs` test only
checks the simple name so it passes regardless.

---

## Vulnerabilities

No memory-safety, path-traversal, or injection vulnerabilities were found in this crate.

### V1 (LOW) — Temp-file staging path is predictable but creation is safe
`vm-cli/src/main.rs:1116-1184`

The non-`.jar` archive staging builds a predictable temp path
(`cratonvm-<pid>-<ms>-<stem>.jar` in `std::env::temp_dir()`). This was a noted arbitrary-write
vector and is **correctly mitigated** with `OpenOptions::create_new(true)` (exclusive create) plus
a 16-attempt collision retry, so a pre-planted symlink/file causes the open to fail rather than
`fs::copy` following/clobbering it. Residual: the staged file is never cleaned up (relies on OS
temp reaping) and predictable names could let a local attacker pre-create-then-race to deny the
staging (DoS only, no integrity loss). Acceptable; flagging for completeness only.

### V2 (LOW) — Unbounded indefinite wait on non-daemon threads with no env override
`vm-cli/src/main.rs:2108-2111`

`wait_for_non_daemon_threads(None)` waits forever by design (JVM-spec). The only escape is the
watchdog `abort()`. This is correct behavior, but combined with the default 120s watchdog it means
a CLI run can be `abort()`ed (rc != 0, core-dump-class exit) rather than exiting cleanly when a
benign non-daemon thread lingers. Not a security issue; UX/exit-code fidelity note.

---

## Stubs and Unimplemented

No `unimplemented!`/`todo!`/`NotImplemented`/synthetic-fake-main stubs in this crate. Items below
are **accept-and-ignore** flags (legitimate launcher behavior, not policy-violating stubs) and one
documented partial feature; listed so the open-sourcing audit has the full picture.

| Location | Item | Note |
|---|---|---|
| `main.rs:804-813` | `-Xms<size>` | Accepted and ignored (heap sized from `-Xmx` only). Honest no-op, but see B1. |
| `main.rs:814-826` | arbitrary `-XX:...` | Silently ignored (MetaspaceSize, GC selectors, etc.). Honest no-op. |
| `main.rs:108-127` | `--enable-native-access` MODULE value | Value accepted-and-ignored; gate is coarse process-wide, not per-module. Documented. |
| `main.rs:734-741` | `-Xbootclasspath/a:` and `/p:` | Append/prepend collapsed to plain replace; boot CP is single ordered list. Documented fidelity gap. |
| `main.rs:1857-1868` | `initPhase2`/`initPhase3` | Not run end-to-end; init level is force-set to 4. Documented; can mask real JDK boot. |
| `main.rs:2124-2142` | unhandled-exception stack trace | Renderer usually finds no `stackTrace[]` (fillInStackTrace only populates the per-thread side map); falls back to `throwable_stack_for`. Documented roadmap item T2.2.18. |
| `gpu` feature, `main.rs:1019-1054` | `--gpu` / `--gpu-info` | In stub builds probe returns NoDriver and the flag self-disables (`args.gpu = false`). Honest, not a fake. |

---

## Performance

The launcher runs once per process, so true hot-path concerns are limited to the
exception-rendering loop. Items below are ordered by impact.

### P1 — Exception-render walks every superclass field list per cause / per STE / per sub-exception
`vm-cli/src/main.rs:2164-2200`, `:2236-2266`, `:2399-2551`

For each exception in the cause chain (and for each `StackTraceElement`, and for each
`PropertyBatchUpdateException` sub-exception and its 6-deep cause chain), the code re-walks the
full superclass hierarchy under a `class_manager.read()` guard to resolve field indices by name.
StackTraceElement field indices are resolved once per array (good), but the Throwable
`detailMessage`/`cause`/`stackTrace`/`target` indices are re-resolved for every cause-chain node.
This is only on the error path, so it is cosmetic for throughput, but it is O(causes × depth ×
fields) string compares. Could cache the four index resolutions for the common
`java.lang.Throwable` layout once. Low priority (error path only).

### P2 — Per-arg `create_java_string` + heap array fill is fine; element loop clones unnecessary
`vm-cli/src/main.rs:1894-1926`

`args.args.iter().map(...).collect::<Vec<Value>>()` then re-iterates to set array elements. Minor;
could write straight into the array. Negligible (argv is tiny). Listed for completeness.

### P3 — `expand_aggregate_jars` lowercases each filename up to 3× per dir entry
`vm-cli/src/main.rs:451-454`

Inside the `read_dir` loop, `name.to_ascii_lowercase()` is computed twice per entry
(`starts_with(split_prefix)` and `!= file_name`). Only runs for a missing `netty-all.jar`, so
effectively never; trivial.

---

## Tests

### Inventory
- **Inline unit tests** (`#[cfg(test)] mod tests`, 56 `#[test]`s): cover `insert_program_args_separator`
  (8), `parse_size` (4), `validate_class_name` (12), `extract_system_properties` (3),
  `normalize_java_launcher_argv` HotSpot rewrites (~15), `extract_hotspot_flags` (5),
  `expand_aggregate_jars` (4), `quarkus_signature_present` (4), plus `--nojit` clap acceptance and
  a full-pipeline `-Xmx` end-to-end-through-clap test. This is genuinely good coverage of the pure
  argv-transformation functions.
- **Integration tests** (`tests/cli_*.rs`, spawn the real binary): helloworld via classpath dir,
  helloworld via classpath jar, `-Xmx` inline/separate/512m compat, main-args ordering,
  missing-class error, uncaught-exception stack trace, `--nojit` env + flag. Plus a workspace-wide
  CI gate (`no_diag_eprintln.rs`) forbidding debug-trace `println!` markers.

### Adequacy / coverage estimate: **~62%**
Basis: the pure functions (`parse_size`, `validate_class_name`, `extract_system_properties`,
`extract_hotspot_flags`, `expand_aggregate_jars`, `quarkus_signature_present`,
`insert_program_args_separator`, most of `normalize_java_launcher_argv`) are well covered by unit
tests. The enormous `run()` function (937-2666, ~1700 LOC) is exercised only indirectly via the 8
end-to-end happy-path integration tests; its many branches — `--jar` manifest path, non-`.jar`
archive staging, Quarkus dir walk, AOT/CDS/JPMS config threading, watchdog arming, the
~500-LOC exception-cause-chain renderer (InvocationTargetException unwrap,
PropertyBatchUpdateException drill-down), `--java-home` validation, `-Xshare`/`-XX:AOTMode` typo
warnings — have **no targeted tests**. `main()`'s panic hook and crash-handler wiring are untested.

**Does not plausibly reach 85%.** The single biggest gap is that `run()` is one giant function
with most of its logic untested and not easily unit-testable as written.

### Most important missing tests
1. **`-Xms <size>` separate-token form** through the full pipeline (would catch B1 today).
2. **`-X` flags other than the handled set** (`-Xss512k`, `-Xint`) reach clap and error — guards B3.
3. **User `--` preservation**: `Main a -- b` should deliver `["a","--","b"]` — guards B2.
4. **`--jar` launch mode**: manifest `Main-Class` resolution + `Class-Path` expansion (currently
   only `--classpath <jar>` is tested, never `--jar <jar>`).
5. **Non-`.jar` archive staging** (`.war`) → temp `.jar` registration and the `create_new`
   collision retry.
6. **Exit-code fidelity**: `System.exit(42)` exits 42; uncaught exception exits 1; bad `--Xmx`
   value exits non-zero with a clear message.
7. **`--java-home` pointing at a non-directory** fails fast (line 1330-1336) — easy and important.
8. **Cause-chain renderer**: an exception with a `Caused by:` chain and the 8-deep truncation
   marker.

---

## Feature Suggestions

1. **Refactor `run()` into testable stages.** Extract config assembly (`Args` + props + hotspot
   flags → `VmConfig`) and the exception renderer into standalone functions taking explicit
   inputs. This alone would let coverage jump past 85% and would isolate B1/B2/B3.
2. **Consolidate the `CRATONVM_*` env sprawl + add discoverability.** 13 distinct env gates are
   read in this file (`DISABLE_JIT`, `DBG_ARGS`, `DBG_EXIT`, `DISABLE_DEFAULT_WATCHDOG`,
   `ENABLE_NATIVE_RING`, `DEFAULT_WATCHDOG_SEC`, `INTRINSIC_STATS`, `STRICT_SWALLOWS`,
   `DBG_CHARSET`, `DBG_ATHROW`, `SYMBOLIZE`, `TEST_SEGV`, `JAVA_HOME`), several duplicating CLI
   flags (`DISABLE_JIT`≈`--nojit`, `DEFAULT_WATCHDOG_SEC`≈`--stack-dump-on-timeout`). Add a
   `--XX:+PrintFlagsFinal`-style or hidden `--list-env` dump, document them in `--help` epilog, and
   promote the user-facing ones (watchdog timeout) to first-class CLI flags.
3. **Round out HotSpot `-X` compatibility** (fixes B3): a single catch-all accept-and-ignore arm
   for unrecognized `-X` flags, matching the existing `-XX:` handling.
4. **`@argfile` support.** Stock `java` reads `@file` argument files (used heavily by long
   classpaths on Windows where the command line length is capped). Surefire/Gradle emit these. The
   pre-clap pipeline is the natural place to expand them.
5. **`JDK_JAVA_OPTIONS` / `JAVA_TOOL_OPTIONS` env-prepend support.** Real `java` prepends these to
   argv. Embedders and CI harnesses rely on them; trivial to add at the top of the argv pipeline.
6. **Exit-code parity audit.** Document and test the mapping: clean `main` return → 0,
   `System.exit(N)` → N, uncaught exception → 1, launcher error → 1, watchdog → `abort()`. The
   watchdog-`abort()` path (rc indistinguishable from a crash) is the one rough edge worth a
   dedicated clean-shutdown-with-nonzero-code alternative.

---

## Files sampled vs fully read

**Fully read:**
- `vm-cli/src/main.rs` (entire 3557 lines, in chunks — including all 56 inline tests)
- `vm-cli/tests/common/mod.rs`
- `vm-cli/tests/cli_helloworld.rs`
- `vm-cli/tests/cli_xmx_compat.rs`
- `vm-cli/tests/cli_main_args.rs`
- `vm-cli/tests/cli_classpath_dir_vs_jar.rs`
- `vm-cli/tests/cli_nojit.rs`
- `vm-cli/tests/cli_missing_class.rs`
- `vm-cli/tests/cli_uncaught_exception.rs`
- `vm-cli/tests/no_diag_eprintln.rs`
- `vm-cli/Cargo.toml`, `vm-cli/README.md`

**Sampled (grep/structure only):** none — the crate is small enough to read in full. Cross-crate
referents (`VmConfig`, `ClassPath`, `agent_loader`, `dispatch_trace`) were treated as black boxes
per scope.
