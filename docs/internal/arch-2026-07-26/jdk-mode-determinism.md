# JDK-mode determinism — ending the host-sniffed standard library

**Date:** 2026-07-26
**Slug:** `jdk-mode-determinism`
**Files changed:** `vm/src/config.rs`, `vm-cli/src/main.rs`
**Status:** landed (default-on, no feature gate, no env gate)

---

## 1. Summary

CratonVM ships **two complete, materially different implementations of the Java
standard library** in one binary, and until this change it picked between them
by sniffing the host machine:

```rust
// vm/src/config.rs:503 (before)
cfg.use_synthetic_jdk = detect_real_jdk().is_none();
```

The same binary, the same command line, the same application — a different
class library, a different set of natives, a different set of bugs, decided by
whether a JDK happened to be on `PATH`. Nothing in the VM's output said which
one ran.

After this change:

| | before | after |
|---|---|---|
| launcher default | host-detected | **real-JDK, always** (`LAUNCHER_DEFAULT_JDK_MODE`) |
| embedding default (`VmConfig::default`) | synthetic | synthetic (unchanged, now *declared* as `EMBEDDED_DEFAULT_JDK_MODE`) |
| selecting the other mode | `--synthetic-jdk` only | `--synthetic-jdk` / `--real-jdk`, symmetric and mutually exclusive |
| selected mode unavailable | silent fallback to the other library | **hard error** naming everything searched |
| role of `detect_real_jdk()` | selection | validation only (`require_real_jdk`) |
| mode visible to the user | nowhere | `-version`, `-Xinternalversion`, panic hook, fatal-error path |

---

## 2. The defect

### 2.1 Two libraries, one silent switch

Per the field doc at `vm/src/config.rs`:

* `use_synthetic_jdk == true` — ~5,200 native Rust stubs are registered; no
  real JDK class files are needed.
* `use_synthetic_jdk == false` — only ~300 truly-native methods are registered;
  real JDK classes load from `$JAVA_HOME/jmods` or the `lib/modules` jimage.

These are not two configurations of one implementation. They are two
implementations. The project's own debugging history is a catalogue of the
divergence: synthetic-vs-realjdk native shadowing, dual `ClassLoader` native
sets, "stub wins by default" surprises, `StringJoiner`'s fake 5-field layout
corrupting the real 7-field object, `Optional.equals` native shadowing, and so
on. A bug report that does not name the mode cannot be triaged; before this
change, no bug report could name the mode, because the VM never printed it and
the user never chose it.

### 2.2 `ARCHITECTURE.md` "Key Design Decision #1" is false for the default build

> *"No JDK dependency. All standard library classes are implemented as native
> methods in Rust. This means no `JAVA_HOME`, no `rt.jar`, and no dependency on
> any JDK installation at runtime."*

The default build reverses this. `vm/Cargo.toml` says so explicitly (NEW-11):
`synthetic-jdk` is deliberately **not** in the default feature set, because
"the default build boots against real JDK bytecode loaded from
`$JAVA_HOME/lib/modules`". The launcher default now matches the Cargo comment
instead of contradicting it on JDK-less machines. (A separate agent owns
`ARCHITECTURE.md`; the correction there is listed in §6.)

### 2.3 The compounding defect: the "fallback" fell back to *nothing*

This is the part that makes the old behaviour worse than merely
non-deterministic, and it was verified in this session:

* The ~5,200 stubs are registered inside `#[cfg(feature = "synthetic-jdk")]`
  in `vm/src/vm/vm_init.rs:1382`.
* `synthetic-jdk` is **not** a default feature of `cratonvm-vm` or
  `cratonvm-cli` (`vm/Cargo.toml:43`, `vm-cli/Cargo.toml:92`).
* Synthetic mode *also* suppresses boot-classpath discovery
  (`vm_init.rs:1037`: `if config.boot_classpath.is_empty() && !config.use_synthetic_jdk`).

So in a stock `cargo build -p cratonvm-cli` binary, on a machine with no JDK,
the old autodetection produced `use_synthetic_jdk = true`, which registered
**no** synthetic stubs (compiled out) **and** discovered **no** boot classpath.
The advertised "graceful fallback to synthetic" was, in the shipping build, a
fallback to a VM with no class library at all — surfacing much later as an
unexplained `NoClassDefFoundError` / `NoSuchMethodError` on the first JDK class
reference. `require_synthetic_jdk()` now rejects that combination at launch.

---

## 3. What changed

### 3.1 `vm/src/config.rs`

New public surface (all additive; `use_synthetic_jdk: bool` is unchanged so
every existing reader — `vm_init.rs`, the ~50 tests that set it literally —
still compiles):

| item | purpose |
|---|---|
| `enum JdkMode { Real, Synthetic }` | the named form of the mode, with `as_str()` (`"real-jdk"` / `"synthetic-jdk"`), `describe()`, `selecting_flag()`, `Display` |
| `const LAUNCHER_DEFAULT_JDK_MODE = JdkMode::Real` | the launcher's fixed default |
| `const EMBEDDED_DEFAULT_JDK_MODE = JdkMode::Synthetic` | the embedding/test default — **declared**, not emergent |
| `const SYNTHETIC_JDK_COMPILED_IN` | `cfg!(feature = "synthetic-jdk")` |
| `VmConfig::for_launcher()` | deterministic launcher config; inspects nothing about the host |
| `VmConfig::jdk_mode()` / `with_jdk_mode()` | named accessors over the bool |
| `detect_real_jdk_from(Option<&str>)` | detection honouring an explicit `--java-home` |
| `describe_jdk_search(Option<&str>)` | renders every probed location and its current value |
| `require_real_jdk(Option<&str>)` | **validation**: `Ok(java_home)` or a complete, actionable error |
| `require_synthetic_jdk()` | rejects synthetic mode in a build without the Cargo feature |

`VmConfig::with_host_jdk_default()` is retained as a thin alias for
`for_launcher()` — it is called from `libcratonvm/src/lib.rs:431` and appears
in `cratonvm-embed` docs, neither of which this change may edit. Its
host-probing behaviour is gone; a test pins that it is now exactly
`for_launcher()`.

`VmConfig::default()` is deliberately **unchanged** (still synthetic). It is
load-bearing: ~49 files under `vm/tests/` build configs from it, and the
in-tree suite must stay hermetic and JDK-free. The difference from the launcher
default is now a stated contract (two named constants, plus an `assert_ne!`
test that fails if they silently converge) rather than something a reader has
to reconstruct from two unrelated code paths.

### 3.2 `vm-cli/src/main.rs`

* `--real-jdk` added; `--synthetic-jdk` and `--real-jdk` are `conflicts_with`
  each other in clap and re-checked in `resolve_jdk_mode` so an argv-
  preprocessing change can't make one silently win.
* `VmConfig::with_host_jdk_default()` → `VmConfig::for_launcher()`.
* The old ad-hoc rule "`--java-home` implies real-JDK mode" is gone; real-JDK
  is the default, so `--java-home` now only *points* the selected mode at a
  specific installation.
* `resolve_jdk_mode()` validates the selected mode and returns the resolved
  `JAVA_HOME`, which is pinned onto the config so the VM's boot-classpath
  discovery resolves the same installation the launcher validated instead of
  re-running the environment probe.
* Version banners (`-version`, `--version`, `-fullversion`, `-showversion`,
  `-Xinternalversion`) are now handled by `version_banner()` ahead of clap.
  clap's built-in `--version` exits before any of our code runs, so it could
  only ever print the crate version — which says nothing about the class
  library. Single-dash forms go to stderr and double-dash forms to stdout,
  matching HotSpot (build tools scrape `java -version` from stderr).

### 3.3 Failure output

`cratonvm --real-jdk` (or the default) on a machine with no usable JDK:

```
real-JDK mode was selected but no usable JDK was found.

Searched, in order:
  --java-home = <not passed>
  CRATONVM_JAVA_HOME = <unset>
  JAVA_HOME = C:\craton\shim  (directory exists)
  java on PATH = <not found>

An acceptable JDK root must contain either:
  * jmods/java.base.jmod   (a full JDK 9+ installation), or
  * lib/modules            (a JRE or jlink-trimmed runtime image, read via the jimage reader)

A JDK 8-style installation with only lib/rt.jar is NOT accepted: CratonVM has no rt.jar boot loader.

Fix by one of:
  * set JAVA_HOME (or CRATONVM_JAVA_HOME, which wins over JAVA_HOME) to a JDK 9+ root;
  * pass --java-home <PATH>;
  * put a `java` launcher from a JDK 9+ installation on PATH;
  * or run the other class library explicitly with --synthetic-jdk (requires a build with the `synthetic-jdk` Cargo feature).

CratonVM does not silently substitute the synthetic class library here: the two
implementations have different semantics and different bugs, so a run whose
library was chosen by the host is not reproducible or reportable.
```

The `rt.jar` rejection is the behaviour pinned by
`detect_real_jdk_returns_none_when_only_rtjar_present`; it now has a companion
test on the validation path (`require_real_jdk_rejects_rtjar_only_install`)
so the rejection can never quietly become a downgrade.

### 3.4 Observability

`cratonvm -version` (stderr):

```
cratonvm version "0.1.0"
CratonVM (build 0.1.0, mixed mode, sharing)
JDK class library: real-jdk — real JDK class files from jmods/ or lib/modules; ~300 native methods in Rust
JDK class library root: C:\Program Files\Java\jdk-25
```

`cratonvm -Xinternalversion` adds:

```
jdk.mode.active                = real-jdk
jdk.mode.default.launcher      = real-jdk
jdk.mode.default.embedded      = synthetic-jdk
jdk.mode.selection             = explicit flag or fixed default (never host-detected)
jdk.mode.synthetic_compiled_in = false
jdk.search:
  ...
```

The mode is also emitted:

* by the launcher panic hook, immediately before the backtrace;
* by `main()`'s `run() returned Err` arm, so every fatal launch carries it;
* at `tracing::info!` on every start, and to stderr under `-verbose:class` /
  `-verbose:gc`.

### 3.5 Tests updated, not deleted

The four tests at the old `vm/src/config.rs:1553-1706` that asserted the
autodetection contract were rewritten to assert the new one:

| old | new |
|---|---|
| `with_host_jdk_default_picks_jmod_when_detected` | `launcher_default_is_real_jdk_when_host_has_a_jdk` |
| `with_host_jdk_default_falls_back_to_synthetic_when_no_jdk` | `launcher_default_never_falls_back_to_synthetic_when_no_jdk` (asserts the mode is unchanged **and** that `require_real_jdk` errors) |
| `explicit_synthetic_jdk_override_wins_over_detection` | `explicit_synthetic_jdk_override_wins_over_launcher_default` |
| `default_config_stays_synthetic_regardless_of_host` | kept, extended to assert the two declared defaults and that they still differ |

Added: `with_host_jdk_default_is_an_alias_for_for_launcher`,
`require_real_jdk_error_names_search_path_and_layouts`,
`require_real_jdk_rejects_rtjar_only_install`,
`require_real_jdk_accepts_jmods_and_honours_explicit_java_home`,
`require_synthetic_jdk_tracks_the_cargo_feature`,
`jdk_mode_strings_are_stable_and_match_cli_flags`. The three
`detect_real_jdk_*` tests are unchanged — detection still works, it just no
longer decides anything.

`vm-cli` gains twelve tests covering flag symmetry, the both-flags error, the
missing-feature error, the no-JDK error text, the banner contents, the
stdout/stderr split, and `-showversion` token stripping.

---

## 4. Behaviour matrix

| invocation | build has `synthetic-jdk`? | JDK on host? | result |
|---|---|---|---|
| `cratonvm Main` | any | yes | real-JDK |
| `cratonvm Main` | any | no | **error** (was: synthetic, i.e. an empty VM in the default build) |
| `cratonvm --real-jdk Main` | any | no | **error** |
| `cratonvm --synthetic-jdk Main` | yes | any | synthetic |
| `cratonvm --synthetic-jdk Main` | no | any | **error** (was: an empty VM) |
| `cratonvm --real-jdk --synthetic-jdk Main` | any | any | **error** |
| `Vm::new(VmConfig::default())` (embedding) | any | any | synthetic (unchanged) |

The only behaviour regressions are the three new errors, and each replaces a
case that previously produced either a silently-different standard library or a
VM with no standard library at all.

---

## 5. Non-goals

`native-builtins` is not deleted and synthetic mode is not removed. That is a
product decision reserved for the human — see the memo in §7.

---

## 6. Required follow-ups in files this change does not own

Each is source-compatible today; nothing below blocks the build.

1. **`vm/src/runtime/crash_handler.rs`** — the hardware-fault (VEH) report is
   the one crash path that does not go through the launcher's panic hook, so it
   currently prints a faulting PC and backtrace with no indication of which
   class library was loaded. One line in the report banner:
   ```rust
   let _ = writeln!(out, "jdk mode: {}", cratonvm_vm::config::JdkMode::from_use_synthetic_jdk(<active config>.use_synthetic_jdk));
   ```
   The handler has no `VmConfig` in scope, so the practical shape is a
   `static ACTIVE_JDK_MODE: OnceLock<JdkMode>` in `crash_handler` published by
   `vm_init` (which does have the config), mirroring the launcher-side
   `ACTIVE_JDK_MODE` added here.
2. **`ARCHITECTURE.md`** — "Key Design Decision #1" must stop claiming there is
   no JDK dependency. The accurate statement is: *two* class-library backends
   exist; the shipped default is real-JDK; the no-JDK synthetic backend is a
   build-time opt-in (`--features synthetic-jdk`).
3. **`docs/CONFIG.md:138`** — the `use_synthetic_jdk` row still documents
   host detection. Replace with: launcher default `real-jdk`
   (`LAUNCHER_DEFAULT_JDK_MODE`), library default `synthetic`
   (`EMBEDDED_DEFAULT_JDK_MODE`), selection via `--real-jdk` /
   `--synthetic-jdk`, unavailable mode is a hard error.
4. **`libcratonvm/src/lib.rs:431`** — still calls
   `VmConfig::with_host_jdk_default()`. It now gets deterministic real-JDK
   mode, which is the intended behaviour, but it performs **no validation**:
   the C embedding API should call `cratonvm_vm::config::require_real_jdk(None)`
   and surface the error through its own error channel rather than booting into
   an empty boot classpath. Recommended: switch to `VmConfig::for_launcher()`
   plus an explicit `require_real_jdk` check.
5. **`cratonvm-embed/src/lib.rs:54`, `cratonvm-embed/README.md:118`,
   `docs/EMBEDDING.md:206`, `docs/book/src/embedding/rust-facade.md:42,84`** —
   doc snippets referencing `with_host_jdk_default()`; the prose at
   `rust-facade.md:84` ("mirrors the CLI launcher: it boots from a …") should
   be reworded to say the mode is fixed, not detected, and the snippets should
   move to `VmConfig::for_launcher()`.
6. **CI** — the synthetic lane must keep building `--features synthetic-jdk`;
   with this change a synthetic run in a feature-off build now fails fast at
   launch instead of failing obscurely later, which will make any mis-wired
   lane loudly visible on the first run.

---

## 7. Decision memo — retire the synthetic path, or keep it?

**The call this memo unblocks:** now that real-JDK is the default and modes are
explicit, is `native-builtins`' synthetic class library worth its cost?

### 7.1 The numbers

Measured in this worktree (2026-07-26):

| metric | value | how |
|---|---|---|
| `native-builtins` crate | **561,755 LoC across 249 `.rs` files** | `find native-builtins -name '*.rs' \| xargs cat \| wc -l` |
| …of which `native-builtins/src` | 513,455 LoC across 136 files | same, `src` only |
| workspace total (excl. two stray root-level `native-builtins-lib.{dev,merge}.rs` snapshots totalling 169,232 lines) | 1,283,368 LoC | `find . -name '*.rs' -not -path './target/*'` |
| **`native-builtins` share of the workspace** | **43.8 %** | 561,755 / 1,283,368 |
| release binary | ~30 MB (as reported in the finding; no release artifact exists in this worktree to re-measure) | `ls -l target/release/cratonvm` |
| build time attributable to `native-builtins` | **not measured** — this session is forbidden from building | `cargo build --release -p cratonvm-cli --timings`, then read the `cratonvm-native-builtins` bar |

Two facts sharpen the picture:

* In the **default build** none of the ~5,200 stub *registrations* execute —
  they are inside `#[cfg(feature = "synthetic-jdk")]` — yet the crate is still
  compiled and linked, because the real-JDK arm calls into it for the ~300
  essential natives plus a set of permanent bridges (`register_concurrent_natives`,
  `register_stamped_lock_natives`, the JMX/`Function$Identity` clusters, the
  SLF4J binder stubs). So the 44 % is paid on every default build and shipped
  in every default binary, while most of it is unreachable at runtime.
* The crate is therefore **not** cleanly separable today: "delete
  `native-builtins`" is not the available move. The available moves are about
  the *stub* population, not the crate.

### 7.2 The case for retiring the synthetic path

* **Divergent bug surface.** Every synthetic stub is a second, independently
  buggy implementation of a JDK class whose real implementation is already
  present in real-JDK mode. The memory index for this project is dominated by
  bugs of exactly this shape — layout mismatches between a stub's fake field
  count and the real class, natives shadowing real bytecode, dual native sets
  per `ClassLoader`, "stub wins by default" surprises. Each one costs a
  debugging session and none of them affect the shipping mode.
* **Two CI lanes.** Every correctness change has to be validated twice, and the
  synthetic lane has already been unbuildable at least once
  (`synthetic-jdk-feature-was-unbuildable-and-untested`). A lane that can rot
  invisibly is worse than no lane.
* **44 % of the codebase for a non-default mode** is a permanent tax on
  compile time, binary size, review surface, refactoring cost, and every
  workspace-wide grep a human does.
* **The design decision it implemented is already reversed.** "No JDK
  dependency" is not what ships. Keeping the implementation of an abandoned
  decision is how `ARCHITECTURE.md` got to be wrong.

### 7.3 The case for keeping it

* **Hermetic tests.** ~49 files under `vm/tests/` and the in-crate suites build
  from `VmConfig::default()`; the synthetic path is what lets the ~5,000-test
  suite run with no JDK, no jimage I/O, and no host coupling. Rebuilding that
  on real-JDK means every test run pays JMOD/jimage loading, and CI machines
  grow a JDK dependency. (Mitigation: a committed jlink-trimmed `lib/modules`
  fixture — the jimage reader already handles it.)
* **No-JDK embedding.** `libcratonvm` / `cratonvm-embed` can, in principle,
  ship a single self-contained binary with no JDK on the target. This is a real
  differentiator, and it is the only capability that is *lost* rather than
  merely *made more expensive* by retirement.
* **Bring-up leverage.** Synthetic stubs are how a missing native gets a
  quick stand-in during bring-up of a new workload.
* **Sunk understanding.** Much of the stub corpus encodes hard-won knowledge
  about exact JDK behaviours; deleting it discards documentation as well as
  code.

### 7.4 Recommendation

**Keep synthetic mode as a build-time opt-in; stop treating it as a coequal
runtime mode; and shrink the stub corpus to the part that is actually used.**
Concretely, in priority order:

1. **Make the compile-time and runtime notions of "mode" the same notion.**
   Today `use_synthetic_jdk` (runtime) and `synthetic-jdk` (compile-time) can
   disagree, and the disagreement was the empty-VM failure of §2.3. This change
   makes the disagreement an error; the next step is to make it unrepresentable
   — e.g. `VmConfig::default()` deriving from
   `cfg!(feature = "synthetic-jdk")`. That was deliberately **not** done here
   because ~49 integration-test files build on `VmConfig::default()` being
   synthetic and this session cannot build or run them. It is a small, safe
   change for a session that can.
2. **Split the crate along the line that already exists in the code.** The
   real-JDK arm of `vm_init.rs` calls a small, enumerable set of
   `register_*` functions (essentials, concurrent, stamped-lock, JMX,
   `Function$Identity`, SLF4J binder, the LBQ `drainTo` bridge). Move exactly
   those into a `native-essentials` crate that the default build depends on,
   and leave the rest behind `synthetic-jdk`. This is the only step that
   actually recovers build time and binary size, and it makes the 44 % figure
   *mean* something: after the split, whatever remains under the feature is
   provably dead weight in the default build.
3. **Run the census before deleting anything.** The tooling already exists:
   `--dump-native-registry` classifies every registered native as
   intrinsic / bridge / synthetic-stub, and `--dump-missing-natives-grouped`
   produces a per-module baseline. Run both across the H2 / Spring / WildFly /
   Tomcat suites in real-JDK mode. Any `SyntheticStub` that is still reached in
   real-JDK mode is a permanent bridge and must move to `native-essentials`
   (this is precisely the mistake the `d8092acb` regression made and had to
   revert — see the comment at `vm_init.rs:1879`). Everything not reached is a
   deletion candidate.
4. **Do not delete the corpus wholesale.** After steps 2–3 the cost of keeping
   the remainder is a feature-gated crate that the default build neither
   compiles into the shipped registration path nor ships bugs from — which is a
   price worth paying for hermetic tests and the no-JDK embedding story.

**What would change this recommendation:** if `--timings` shows
`native-builtins` is a small fraction of build wall-clock, the cost side
collapses and the answer is plainly "keep, feature-gated, split anyway for
binary size". If instead it dominates the build, step 2 becomes urgent rather
than merely correct. That single measurement is the cheapest next action and it
is the one thing this session could not perform.
