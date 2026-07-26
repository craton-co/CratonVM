# Startup cost and crash diagnostics

**Date:** 2026-07-26
**Slug:** `startup-and-diagnostics`
**Files changed:** `vm/src/vm/vm_init.rs`, `vm/src/runtime/crash_handler.rs`,
`libcratonvm/src/lib.rs`, `docs/CONFIG.md`
**Status:** landed (default-on, no feature gate, no env gate)

**Basis:** `arch/wave1-integration-20260726` merged at
**`1081aa2c28cbfe88c08ceefede79a95933af8565`** (= `dev` @ `6495a191c` plus
fourteen agent branches). Every line number below is given twice where it
matters — *as merged* (before this change) and *as landed* — because this
change itself shifts `vm_init.rs`. Greppable strings accompany each so they
can be re-found after further drift.

---

## 1. Summary

Three pieces of work, in one place because they share the same failure mode:
*the VM knew a fact, and did not say it.*

| | before | after |
|---|---|---|
| hardware-fault report names the class library | no | yes (Windows VEH + Unix signal handler + panic hook) |
| hardware-fault report names the collector / moving verdict | no | yes |
| hardware-fault report names JIT state | partial (faulting method name only) | quiescence depth, code-range count, per-thread non-moving pins |
| crash report carries Java frames | no | yes (primordial thread) |
| C-ABI embedding validates the JDK mode | **no** | yes — `require_real_jdk` / `require_synthetic_jdk`, hard error |
| C-ABI embedding can select a mode | no | `--real-jdk` / `--synthetic-jdk`, launcher spelling and rules |
| boot-time instrumentation | none | three `tracing::info!` phase spans + a total |
| `docs/CONFIG.md` JDK-mode row | documents host detection | documents the deterministic contract |

---

## 2. Where boot time actually goes

**Measured, not guessed.** No build was possible in this session (nine
concurrent agents), so the measurements below are (a) direct reads of the code
that runs at boot, and (b) an out-of-process measurement of the *dominant*
cost — the JMOD ingestion — reproduced against the same JDK and the same
algorithm the VM uses. Each claim states which kind it is.

### 2.1 The dominant cost: every JMOD is fully inflated at `ClassManager::new`

`SharedVm::new` builds the class manager before any Java code runs:

* `vm/src/vm/vm_init.rs` — *merged* `:565`, *landed* `:587`
  (`let mut class_manager = ClassManager::new(&boot_cp, &ext_cp, &config.classpath);`)
* → `classloading/src/class_manager.rs:1712` `ClassManager::new` →
  `BootstrapClassFinder::new(boot_classpath)` → `ClassPath::new`
  (`classloading/src/class_path.rs:1372`)
* → for every `.jmod` path, `ClassPath::load_jmod`
  (`classloading/src/class_path.rs:1557`, definition at `:4271`)

`load_jmod` does not index the archive. It **decompresses every entry under
`classes/` into an in-memory `HashMap`** (`class_path.rs:4316-4335`,
`// Pre-extract all class entries into an in-memory cache`), then re-opens the
archive a second time for non-class resources (`:4344`).

And `discover_boot_classpath` (`vm/src/config.rs:913`) puts **every** `.jmod`
in `$JAVA_HOME/jmods` on the boot classpath — `java.base.jmod` first, then a
sorted scan of the rest (`config.rs:946-958`) — explicitly so that "any JDK
class can be resolved on demand".

The two together mean: *a default real-JDK boot eagerly inflates the entire JDK
before loading a single class.*

**Measurement** (out-of-process, `C:\Program Files\Microsoft\jdk-25.0.3.9-hotspot`,
the JDK on this host):

| quantity | value |
|---|---|
| `.jmod` files in `jmods/` | **70** |
| on-disk size of `jmods/` | 84.0 MB |
| entries under `classes/` across all 70 | **27,962** |
| compressed size of those entries | 60.2 MB |
| **decompressed size of those entries** | **136.0 MB** |
| wall-clock to inflate all of them into a `HashMap`, optimised native zlib | **15.0 s / 20.9 s** (two runs) |

Method: strip each JMOD's 4-byte `JM\x01\x00` prefix, open the remainder as a
ZIP, and inflate every `classes/…` entry into a map — i.e. exactly
`load_jmod`'s loop. The inflate was run through .NET's `System.IO.Compression`
(native zlib) rather than Rust, so it is a *floor*, not a prediction:
`load_jmod` additionally allocates a `String` key and a `SharedBytes` value per
entry, and CratonVM's own comment on `load_jmod` records that deflate in a
debug build is catastrophically slower still ("~30s for 200 classes vs <2s with
pre-extraction", `class_path.rs:4267`).

Two consequences worth stating plainly:

1. **~136 MB of decompressed bytecode is resident before `main`**, of which a
   typical workload touches classes from perhaps five modules.
2. The pre-extraction was introduced *as* a performance fix — and it is one, for
   per-lookup cost. The defect is not pre-extraction; it is that it is applied
   to all 70 modules unconditionally instead of to the modules a run actually
   opens.

### 2.2 The same JDK offers a lazy path that boot ordering never reaches

`ClassPath::load_jimage` (`class_path.rs:4095`) walks `lib/modules` **for entry
names only** (`reader.iter_entries()` → three name→module index maps,
`:4099-4135`). No class bytes are decompressed; they are read on demand.

`discover_boot_classpath` checks `jmods/` **first** and only falls back to
`lib/modules` when `jmods/` is absent (`config.rs:934` then `:977`). This host's
JDK has both (`lib/modules` is 138.2 MB). So the stock configuration takes the
eager path and the lazy path is reachable only on JRE / jlink-trimmed images.

This is the single largest available startup win, and it is a **cross-owner
request** (§6.1) — the ordering lives in `vm/src/config.rs`, and flipping the
default reader for every boot is not a change to land without being able to run
a single test.

### 2.3 Eager classpath scanning: the historic O(jars × zip-probes) hang is closed

`ClassManager::new` also scans every boot/ext/app classpath entry for
`module-info.class` (`class_manager.rs:1739-1753`, `scan_module_infos` at
`class_path.rs:3871`). This is the shape that used to hang, but it no longer
probes:

* JAR entries carry an `entry_index: FxHashSet<String>` built once at load by a
  **central-directory name walk with no decompression**
  (`build_archive_entry_index`, `class_path.rs:1319`), and lookups are `O(1)`
  set hits (`find_in_indexed_archive`, `:1329`).
* JMOD entries answer from the `classes_cache` that `load_jmod` already
  inflated — free at this point, having been paid for in §2.1.

So module scanning is cheap. It is worth being explicit that this specific
documented history is *closed*, so nobody re-optimises it.

### 2.4 How many classes are loaded before `main`

`ClassManager::bootstrap_core_classes` (`class_manager.rs:2311`) loads a
hard-coded list in eight tiers — core, extended, exceptions, collections,
collection *views* and iterators, functional interfaces, io, internal — and each
`load_class` recursively pulls supertypes and interfaces.

* **323** explicitly named classes (counted over the eight arrays,
  `class_manager.rs:2318-2680`).
* The *resolved* total is strictly larger and was not previously reported at
  all. It is now: the new phase-2 log line prints
  `class_manager.loaded_count()` alongside the named count, so "how many classes
  are loaded before `main`" is answerable from any `RUST_LOG=info` run instead of
  by instrumenting a build.

`vm_init.rs` adds ~14 more eager `load_class` calls and 3
`ensure_synthetic_class` calls of its own (the `Enumeration$Impl` /
`Comparator$Native` / unmodifiable-view wiring, *merged* `:648-780`), each with
a specific documented bug behind it — e.g. `java/lang/AssertionError` is loaded
purely so that assert-bearing methods stay JIT-compilable (*merged* `:673`). All
are individually cheap.

### 2.5 Native registration: the tables are mode-independent, the population is not

Two separate questions, two different answers.

**Do the tables get sized for the synthetic corpus even in real-JDK mode?**
Yes, and it is fine. `NativeMethodRegistry::new`
(`native-api/src/registry.rs:3931`) pre-sizes four collections to a fixed
`BOOT_REGISTRATION_HINT = 4096` — `slots`, `slot_by_key`, `registrations`,
`by_method_desc`, plus `name_index` — with the comment "~3,100 native methods
are registered at boot". This is unconditional, but it is one bounded
allocation of a few hundred KB, not per-native work. Not a startup problem.

**Is the population really ~300 in real-JDK mode?** *No — the documented figure
does not survive contact with the code.* `vm/src/config.rs`'s `use_synthetic_jdk`
field doc says real-JDK mode registers "only the ~300 truly-native methods", and
`jdk-mode-determinism.md` repeats it. But the `#[cfg(not(feature = "synthetic-jdk"))]`
arm (*merged* `vm_init.rs:1423`, *landed* `:1493`) calls
`register_essential_natives` and roughly forty further top-level `register_*`
cascades (`register_concurrent_natives`, `register_collections_natives`,
`register_io_natives`, `phases_late::register_phase57_nio_file`, the JMX
cluster, the `lang_invoke` cluster, …). And `register_essential_natives` alone
(`native-builtins/src/lib.rs:6851-19477`, 12,627 lines) contains **960 direct
`registry.register(` call sites** and calls **182 distinct sub-registrar
functions**.

So the real-JDK native surface is thousands of registrations, not hundreds. The
"~300" number is an *intrinsics* count that has been re-used as a
*registrations* count. This matters beyond documentation accuracy: the
`jdk-mode-determinism.md` §7.4 recommendation to split a `native-essentials`
crate out of `native-builtins` is sized against that number, and the split is
much larger than "~300 methods" implies. Anyone acting on §7.4 should run
`--dump-native-registry` first (as §7.4 step 3 already says) rather than trust
the figure.

The exact count is now printed at boot rather than estimated: the new phase-3
log line reports `native_methods.len()` after the last cascade, for whichever
`cfg` arm was compiled in. Registration itself does no per-call environment
lookups — `real_net_sockets_enabled()` / `real_forkjoinpool_enabled()`
(`registry.rs:24`, `:53`) are `OnceLock`-cached — so the cost is proportional to
the count and nothing else.

### 2.6 What landed

`vm_init.rs` gains three phase timers and a total, all on `tracing::info!` —
the channel the surrounding boot-classpath logging already uses, so there is no
new flag and nothing is default-off:

| line (landed) | emits |
|---|---|
| `:587-621` | `boot phase 1/3 classpath ingestion: <dur> (N boot entries, M ext, K app)` |
| `:684-701` | `boot phase 2/3 core-class bootstrap: <dur> (N named classes resolved to real bytecode, M classes in the ClassStore)` |
| `:2464-2476` | `boot phase 3/3 native registration: <dur> (N natives registered)` |
| end of `SharedVm::new` | `boot: SharedVm::new total <dur> (phase 1 …, phase 2 …)` |

Cost is four `Instant::now()` calls per VM construction. The existing opt-in
`[CP] ClassPath::new call#N … elapsed=` diagnostic
(`class_path.rs:1373`, `loader_flags().dbg_classpath`) still gives the
finer-grained per-`ClassPath` breakdown; the phase lines are the always-computed
summary that makes a regression visible without anyone having to know that flag
exists.

`SharedVm::new` is not all of startup — `System.initPhase1/2/3` and application
class loading follow, driven from `vm-cli`'s bootstrap loop — but it is the part
that is fixed cost for every run.

---

## 3. Crash diagnostics

### 3.1 The gap

`install_hardware_fault_handler` (`crash_handler.rs`, *merged* `:1159`) is the
**one crash path that does not go through the launcher's panic hook**. On
Windows a hardware fault is a structured exception, so the VEH at
`windows_fault::vectored_handler` (*merged* `:588`) is the only thing that runs.
It printed the faulting PC, an access breakdown, raw and symbolized native
frames, the GPRs, and several targeted memory windows — a genuinely good report
— but it named neither the class library, nor the collector, nor the JIT's
state, nor a single Java frame.

Every one of those omissions has a documented cost in this project:

* **Class library.** Two complete, differently-buggy standard libraries
  (`jdk-mode-determinism.md` §2.1). A report that does not name the mode cannot
  be triaged. §6.1 of that document raised exactly this follow-up.
* **Collector mode.** The default generational young collection silently
  degrades to a **non-moving sweep** whenever a live JIT frame cannot prove a
  complete rewritable root map (`gc/src/gc_quiescence.rs:468`,
  `record_moving_young_coverage_fallback`; see
  `moving-young-precise-roots.md`). Heap corruption and stale-`ObjectRef`
  reports read completely differently depending on which happened.
* **JIT state.** Separates a codegen bug from an interpreter/GC bug on the first
  read.

### 3.2 The design: publish lock-free, read lock-free

A crash handler must never block on a lock the faulting thread was already
holding. So nothing here reaches into the live `Vm`. A new
`VM diagnostic snapshot` section in `crash_handler.rs` (landed `:99-379`) holds:

| cell | kind | published by |
|---|---|---|
| `ACTIVE_JDK_MODE_CODE` | `AtomicU8` | `publish_jdk_mode` |
| `ACTIVE_JDK_MODE` | `OnceLock<(JdkMode, Option<String>)>` | `publish_jdk_mode` |
| `ACTIVE_GC_ALGORITHM` | `OnceLock<&'static str>` | `publish_gc_algorithm` |
| `PRIMORDIAL_FRAME_TRACE` | `OnceLock<Arc<Mutex<Vec<StackTraceEntry>>>>` | `publish_primordial_frame_trace` |

Every reader is a `OnceLock` load, a relaxed atomic load, or a `try_lock` that
reports failure instead of waiting. The GC and JIT verdicts are not published at
all — they are read live from counters that are already plain atomics
(`gc_quiescence::moving_young_cycle_count` / `…_coverage_fallback_count` /
`…_incomplete_reason` / `force_non_moving_jit_roots` /
`unregistered_jit_frame_on_stack` / `depth`, and
`cratonvm_jit::jit_code_range_count` / `…_ranges_generation` /
`lookup_jit_method_name`).

The mode is kept in **two** representations on purpose. `ACTIVE_JDK_MODE_CODE`
exists so the *Unix async-signal handler* can emit it: `jdk_mode_bytes()` is a
single relaxed load returning one of three `&'static [u8]` literals — no
allocation, no lock, no formatting — which is the only thing that module's
async-signal-safety discipline (`crash_handler.rs:11-44`) permits. Both cells
are **first-write-wins**: `OnceLock::set` already ignores a second publish, so
an unconditional `store` on the atomic would let a test fixture's second VM make
the byte form contradict the `OnceLock` form. A report that disagrees with
itself about the class library is worse than one that omits it.

### 3.3 What the report now carries

Publication sites, all in `vm_init.rs` (landed):

* `:475-495` — `publish_jdk_mode(config.jdk_mode(), config.java_home)` and
  `publish_gc_algorithm(…)`, at the top of `SharedVm::new`. This is the one
  place *every* boot passes through with the config in scope — launcher,
  `libcratonvm`, `cratonvm-embed`, and every in-tree test alike.
* `:4953-4966` — `publish_primordial_frame_trace(main_thread.frame_trace.clone())`,
  beside the existing `ThreadRegistry::set_frame_trace(ThreadId(0), …)`.

Consumption sites in `crash_handler.rs` (landed):

* `CrashReport::write_header` — `# jdk mode: …`. In the **header**, not only in
  the new section, because truncated reports lose the tail first.
* `CrashReport::write_vm_section` — a new `V M  S T A T E` block between the
  thread and process sections, carrying `vm_diagnostic_lines(None)`.
* `windows_fault::vectored_handler` — the same `vm_diagnostic_lines`, with the
  faulting PC passed in, immediately after the faulting-method line and before
  the native frame list.
* the Unix `crash_signal_handler` — `jdk mode: <bytes>` on both stderr and the
  short `hs_err` marker file.
* `get_vm_state()` — previously the literal string
  `"crash state — detailed VM info unavailable"`; now reports the mode and
  collector, which are in fact available.

Rendered content:

```
jdk mode: real-jdk (java.home=C:\Program Files\Microsoft\jdk-25.0.3.9-hotspot)
gc collector: generational
gc young-gen policy: non-moving (STW mark-sweep young)
gc young-gen actual: 0 moving cycle(s), 7 cycle(s) diverted to the NON-MOVING sweep
gc young-gen last incomplete-coverage reason: <label>
gc young-gen: force-non-moving-jit-roots is ARMED on the faulting thread …
jit: guarded compiled frames live process-wide: YES (quiescence depth=2)
jit: 148 compiled code range(s), cache generation 3
jit: faulting pc is inside compiled method com/example/Foo.bar
Java frames (primordial thread, 12 frame(s), published at the last blocking/safepoint deposit — may lag the faulting instruction):
  at com/example/Foo.bar(Foo.java:41)
  …
```

Three honesty constraints were deliberately encoded in the wording, because a
diagnostic that overstates what it knows is worse than none:

* **policy vs actual.** `moving_young_enabled()` is the configured policy; the
  two counters are what really happened. Printing only "generational" would not
  distinguish a compacting young generation from one permanently diverted to the
  sweep — which was precisely how the degrade stayed invisible.
* **process-wide vs per-thread.** `gc_quiescence::depth()` is a *global* count of
  guarded JIT entries, while `force_non_moving_jit_roots()` and
  `unregistered_jit_frame_on_stack()` are thread-locals cleared at the start of
  every root-gathering pass. The labels say which is which; "depth=3" would
  otherwise read as three compiled frames under the faulting thread.
* **deposited vs live.** `frame_trace` is republished at blocking/safepoint
  deposit points (`vm_exec.rs:2494`), so for a thread parked in a native call it
  is exact and for a thread crashing mid-loop it is the last known good
  position. The line says so rather than implying a live walk.

Every branch that cannot produce data still produces a *line* — `<not published
— crashed before VM construction>`, `<frame-trace mutex was held at crash time;
not waiting on it>`, `<none published yet>`. A silently absent Java stack is
indistinguishable from a thread with no Java frames.

### 3.4 Tests

Ten `#[cfg(test)]` tests in `crash_handler.rs`, written to be order-independent
(the publication cells are process-global, so a test that *required* an
unpublished state would break under `cargo test`'s shared binary):

`jdk_mode_bytes_is_one_of_three_static_literals` (the signal-safe path must
never allocate or panic at any publication state),
`jdk_mode_line_never_silently_omits_the_mode`,
`publish_and_report_are_consistent` (pins first-write-wins **and** that the two
representations cannot diverge), `gc_state_lines_report_collector_and_the_moving_verdict`
(pins the policy/actual split), `jit_state_lines_attribute_a_faulting_pc` (pins
that a missing pc produces *no* attribution line rather than a fabricated one),
`java_stack_lines_never_block_and_always_explain_themselves`,
`crash_report_carries_the_vm_state_section`,
`crash_report_header_names_the_class_library` (asserts against the substring
*before* the thread-section marker, so a refactor cannot quietly drop the
duplication), `vm_state_summary_reports_mode_and_collector`.

---

## 4. `libcratonvm`: the C embedding API validated nothing

`config_from_args` (*merged* `libcratonvm/src/lib.rs:431`) called
`VmConfig::with_host_jdk_default()` and performed **no validation** — the
follow-up raised as `jdk-mode-determinism.md` §6.4. After that change the alias
is deterministic real-JDK, which is the intended default, but an embedder on a
host with no usable JDK still booted with an empty boot classpath and found out
much later as an unexplained `NoClassDefFoundError`.

Landed:

* **`validate_jdk_mode(cfg)`** — the C-ABI counterpart to `vm-cli`'s
  `resolve_jdk_mode` (`vm-cli/src/main.rs:1687`). Calls `require_real_jdk` /
  `require_synthetic_jdk` and, on success in real mode, **pins the resolved
  `JAVA_HOME` onto the config** so the VM's boot-classpath discovery resolves
  the installation that was just validated instead of re-running the environment
  probe. Applied on all three exits of `config_from_args` (null `args`,
  `nOptions == 0`, and the end of the option loop), so no path escapes it.
* **`InitArgsError::JdkModeUnavailable(String)`** — a new variant carrying the
  full actionable message. Both entry points (`JNI_CreateJavaVM` at `:619`,
  `cratonvm_create` at `:1164`) already render `InitArgsError` through their own
  error channel (`JNI_ERR` + `set_last_error`), so the text reaches the embedder.
* **`--real-jdk` / `--synthetic-jdk` as JavaVM options**, with the launcher's
  spelling and the launcher's mutual-exclusion rule (passing both is an error,
  and `ignoreUnrecognized` does *not* soften it — the flags were recognized,
  they just contradict). Without this the change would be a silent capability
  regression: host autodetection used to hand a JDK-less embedder the synthetic
  library, and the deterministic default alone would leave no way to ask for it.

**Test hermeticity.** Validation makes `config_from_args` host-dependent, so the
existing `config_from_args_honors_ignore_unrecognized` would have started
passing or failing according to whether the machine happened to have a JDK — the
exact non-determinism this whole line of work exists to remove. The test module
gains a `ScratchJdk` fixture plus `with_fake_jdk` / `with_no_jdk` helpers
(serialised on a mutex, stashing and restoring `JAVA_HOME` /
`CRATONVM_JAVA_HOME`), mirroring `vm/src/config.rs`'s detection tests —
including their reason for pointing `CRATONVM_JAVA_HOME` at an *empty real
directory* rather than emptying `PATH` (on Windows `CreateProcess` can still
resolve a system `java` with an empty `PATH`). No new dev-dependency was added;
the fixture uses `std::env::temp_dir()` and cleans up on `Drop`.

Six new tests: `config_from_args_defaults_to_real_jdk` (and that it agrees with
`LAUNCHER_DEFAULT_JDK_MODE`), `config_from_args_pins_the_validated_java_home`,
`config_from_args_fails_loudly_when_no_jdk_is_available` (asserts the message is
*actionable*, not just a verdict), `config_from_args_rejects_both_mode_flags`,
`config_from_args_honours_the_synthetic_flag_subject_to_the_cargo_feature`
(branches on `SYNTHETIC_JDK_COMPILED_IN`, and deliberately runs with no usable
JDK to pin that synthetic mode consults the host not at all),
`config_from_args_honours_the_real_flag`.

---

## 5. Re-verification of the `jdk-mode-determinism.md` headline finding

The claim: *a stock default-feature build that autodetected into synthetic mode
got neither the stubs nor a boot classpath.* Re-derived on the merged tree
(`1081aa2c2`), all four legs hold.

| leg | merged line | landed line | greppable |
|---|---|---|---|
| stub registration is feature-gated | `vm_init.rs:937` | `:1007` | `#[cfg(feature = "synthetic-jdk")]` |
| …and the stubs are inside it | `vm_init.rs:941` | `:1011` | `register_builtins(&mut native_methods);` |
| real-JDK-only arm | `vm_init.rs:1423` | `:1493` | `#[cfg(not(feature = "synthetic-jdk"))]` |
| synthetic mode also suppresses boot-classpath discovery | `vm_init.rs:539` | `:562` | `let boot_cp = if config.boot_classpath.is_empty() && !config.use_synthetic_jdk {` |

And `synthetic-jdk` is still **not** a default feature:

* `vm/Cargo.toml:37` — `default = ["awt", "experimental-tls", "experimental-jmx", "experimental-serialization", "experimental-aot", "experimental-debug", "zgc"]`; the feature itself at `:43`.
* `vm-cli/Cargo.toml:79` — `default = ["mimalloc"]`; the feature at `:92`.

**The guard now rejects the combination.** `require_synthetic_jdk`
(`vm/src/config.rs:1162`) returns an error whenever `SYNTHETIC_JDK_COMPILED_IN`
is false, and it is reached on every path that can select synthetic mode:

* the launcher — `vm-cli/src/main.rs:1715`, inside `resolve_jdk_mode` (`:1687`),
  which `run()` calls before constructing the VM (`:2283`);
* the C embedding API — `libcratonvm/src/lib.rs`, `validate_jdk_mode`, added by
  this change.

The remaining hole is direct Rust embedding via `Vm::new(VmConfig::default())`,
which is synthetic by design and by declaration (`EMBEDDED_DEFAULT_JDK_MODE`)
and is what keeps the in-tree suite hermetic. Closing it means making the
compile-time and runtime notions of "mode" the same notion — already recorded
as `jdk-mode-determinism.md` §7.4 step 1, and still the right next step.

---

## 6. Cross-owner requests

These are **not** made by this change. Each names the exact file, function and
rationale.

### 6.1 `vm/src/config.rs` — prefer the lazy jimage over eager JMOD inflation

**File / function:** `vm/src/config.rs:913`, `discover_boot_classpath`.

**Request:** when `$JAVA_HOME/lib/modules` exists, return it *instead of*
enumerating `jmods/*.jmod` — i.e. move the `lib/modules` check (currently
`:977`, after the jmods scan) ahead of the jmods branch (`:934`), or gate the
jmods branch on `lib/modules` being absent.

**Rationale:** measured in §2.1 — the jmods path eagerly decompresses 27,962
class entries / **136 MB** across 70 modules on a stock JDK 25, costing **15-21 s
of pure inflate** even with optimised native zlib, before a single Java class is
loaded. `load_jimage` (`class_path.rs:4095`) builds name indexes only and reads
class bytes on demand. Both readers are already in production use — the jimage
path is what JRE and jlink-trimmed images take today, and `RKC16N.9` records it
being added precisely because the boot classpath must not come back empty.

**Why not landed here:** this changes which reader serves class bytes on the
default boot path for every run, and this session could neither build nor run a
single test. It needs a session that can run the H2 / Spring / Tomcat suites in
real-JDK mode both ways. If the switch proves risky, the equal-value fallback is
to make `load_jmod` build a name index (as `build_archive_entry_index` already
does for JARs, `class_path.rs:1319`) and inflate lazily per class — same win,
confined to `classloading`, but a larger change.

### 6.2 `vm/src/config.rs` — the "~300 natives" figure is wrong

**File / functions:** `vm/src/config.rs` — three places say "~300":

* the `use_synthetic_jdk` field doc, around `:305-311`;
* the `JdkMode::Real` variant doc, `:88-91` ("Only the ~300 truly-native
  methods are Rust");
* **`JdkMode::describe()`, `:113-121`** — which is *user-visible*: it is what
  `cratonvm -version` prints on the `JDK class library:` line.

**Request:** replace with a measured figure, or with a pointer to the boot log
line that now prints it. §2.5 shows `register_essential_natives` alone
(`native-builtins/src/lib.rs:6851-19477`) has 960 direct `registry.register(`
call sites and calls 182 distinct sub-registrars, before the ~40 further
top-level cascades the real-JDK arm invokes.

**Rationale:** `jdk-mode-determinism.md` §7.4 step 2 sizes a proposed
`native-essentials` crate split against this number. The split is materially
larger than "~300 methods" suggests, and the recommendation should be re-read
with the real figure. (§7.4 step 3 already prescribes running
`--dump-native-registry` first; this is a reason to actually do it.)

### 6.3 `vm/src/vm/vm_exec.rs` — publish worker-thread frame traces to the crash handler

**File / functions:** `vm/src/vm/vm_exec.rs:7002` (platform-thread registration,
`set_frame_trace(tid, jvm_thread.frame_trace.clone())`) and `:6932` (virtual-thread
runtime, `set_frame_trace(tid, virtual_runtime.frame_trace.clone())`); also
`vm/src/native/jni.rs:462` for JNI-attached foreign threads.

**Request:** at each of those three sites, additionally publish the same `Arc`
into a per-OS-thread cell readable by `crash_handler`, so
`crash_handler::java_stack_lines` can render the *faulting* thread's frames
rather than only the primordial thread's. The primordial publication landed in
this change at `vm_init.rs:4953-4966`; the shape to mirror is
`crash_handler::publish_primordial_frame_trace`, generalised to a
`thread_local!` handle plus a registry of live handles, or simply a
`thread_local!` `Arc` clone that `java_stack_lines` reads directly (which is
strictly safer — no shared registry to lock).

**Rationale:** most crashes are on the primordial thread, so the landed version
covers the common case, but the virtual-thread resume heap-corruption class and
the STW-takeover deadlock class both fault on workers, and those are exactly the
reports where a Java stack decides the diagnosis. `try_lock`-only access keeps
it as safe as the landed path.

### 6.4 `ARCHITECTURE.md` — "Key Design Decision #1"

Already raised as `jdk-mode-determinism.md` §6.2 and still open. Restated here
only because §2.1's measurement gives it a second, independent edge: the
document claims "no dependency on any JDK installation at runtime", while the
shipped default not only requires a JDK but eagerly ingests 136 MB of it at
boot.

### 6.5 CI — keep the synthetic lane building `--features synthetic-jdk`

Already raised as `jdk-mode-determinism.md` §6.6. Reinforced by §4: the C
embedding API now also fails fast when synthetic mode is requested from a
feature-off build, so a mis-wired lane is loudly visible on the first run of
either surface.

---

## 7. Non-goals

* No environment-variable gate was added, and nothing landed default-off. The
  boot timers use `tracing::info!`; the diagnostic snapshot is unconditional.
* The `native-builtins` split (`jdk-mode-determinism.md` §7.4) is untouched —
  §6.2 only corrects the number it is sized against.
* Startup was measured, not optimised. The one large available optimisation is
  §6.1 and it belongs to another owner and another session — one that can run
  the suites.
