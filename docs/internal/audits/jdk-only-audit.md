# JDK-only mode — reproducible audit

| | |
|---|---|
| **Status** | Wave 1 (measurement). The static half and the runtime-census half both run today; `scripts/jdk-only-census.sh` is the supported driver for the runtime half. |
| **Normative source** | [`feature-designs/jdk-only-mode.md`](feature-designs/jdk-only-mode.md) — the interface contract. This document does not define semantics; it defines how to *measure* them. |
| **Companions** | [`jdk-only-runtime-services.md`](../../known-issues/jdk-only/runtime-services-blocker-inventory.md) (blocker inventory) · [`jdk-only-native-review.md`](jdk-only-native-review.md) (promotion checklist) · [`jdk-only-ambient-category-audit.md`](jdk-only-ambient-category-audit.md) · [`known-issues/jdk-only/`](known-issues/jdk-only/) · [`benchmarks/jdk-only.md`](benchmarking/jdk-only.md) |
| **Tooling** | `scripts/jdk-only-census.sh` · [`tools/jdk-only-blockers/`](../tools/jdk-only-blockers/README.md) |

The purpose of this audit is to produce a **runtime-observed** inventory of every
compatibility substitution the VM performs, with provenance.

## Why a source grep is not the source of truth

Three properties of this repository, not of the report that proposed the work:

1. **The native kind is ambient, not per-call — and this is the load-bearing
   reason.** `NativeMethodRegistry::register` takes **no kind argument**. The
   kind comes from the registry's current category, set by
   `NativeMethodRegistry::set_category` / `with_category`
   (`native-api/src/registry.rs`), and it **defaults to `SyntheticStub`**. A
   `register()` call that is not inside a `with_category(...)` block is therefore
   a synthetic stub *by omission* — it has no syntactic marker at all. A grep for
   `NativeKind::SyntheticStub` cannot see it, so **a grep-based census
   undercounts by construction**. Only the booted registry knows the answer,
   which is why every count in this document comes from a runtime dump.
2. **Registrations overwrite one another.** `register` is called thousands of
   times and later entries replace earlier ones for the same
   `(class, name, descriptor)` key. `vm/src/vm/vm_init.rs` deliberately
   re-registers some implementations *after* the broad collection passes. Only
   the surviving entry decides behaviour; the census's `overwrote` column is
   what makes the displaced ones visible.
3. **Some classes are fabricated at runtime.** `ClassManager::ensure_synthetic_class`
   and the enterprise-prefix fallback in `ClassManager::load_class`
   (`classloading/src/class_manager.rs`) create classes that exist in no source
   file, so no file-level search can enumerate them.

> **Citation convention.** This document cites **file + symbol**, not
> `file:line`. Implementation is in flight across many agents in the same
> checkout and line numbers drift within the hour. Use `git grep -n '<symbol>'`
> to locate the current line.

---

## 1. Pinned-commit setup

Every artifact below must be attributable to one commit. Run this first and keep
the output with the artifacts.

```bash
# From the repository root. Read-only: no checkout, no pull — audit whatever
# tree you are actually testing, and record which one that was.
git rev-parse HEAD            | tee target/jdk-only-audit.commit
git status --porcelain        | tee target/jdk-only-audit.dirty
git rev-parse --abbrev-ref HEAD

mkdir -p target/jdk-only-audit

cargo metadata --format-version 1 --no-deps \
  > target/jdk-only-audit/cargo-metadata.json

git ls-files > target/jdk-only-audit/files.txt
```

`target/jdk-only-audit.dirty` must be empty for a *citable* baseline. A dirty
tree is fine for iteration, but do not compare a dirty census against a clean
one.

> **Deviation from the research report.** The report opens with
> `git checkout main && git pull --ff-only`. Do not do that here. This is a
> shared checkout whose branches move under concurrent sessions, and
> `git checkout`/`git pull` will discard or relocate another agent's work.
> Record the commit; do not move to it.

---

## 2. Static inventory

The static half is **orientation, not measurement** — see the ambient-kind
argument above. It tells you which files to read; the runtime census tells you
what is true. Run each category separately: they answer different questions and
must not be merged into one file.

```bash
mkdir -p target/jdk-only-audit

# (a) Native classification and the ambient-category sites that decide it.
#     `set_category` / `with_category` matter more than the enum literals: the
#     registry's default category is SyntheticStub, so the *absence* of a
#     category block is what creates most stubs.
rg -n --hidden -g '!target/**' \
  'NativeKind::(SyntheticStub|Bridge|Intrinsic)|with_category|set_category' \
  types native-api native-builtins native-builtins-crypto \
  native-builtins-security native-collections native-io vm \
  > target/jdk-only-audit/native-kind-sites.txt

# (b) Synthetic class creation, the stub-upgrade path, and the layout tables
#     that native code assumes about fabricated objects.
rg -n --hidden -g '!target/**' \
  'ensure_synthetic_class|create_synthetic_stub|is_synthetic_stub|synthetic_stub_fields|synthetic_stub_instance_field_count|is_enterprise_stub_prefix|is_native_backed_jdk_stub' \
  classloading vm native-builtins \
  > target/jdk-only-audit/synthetic-class-sites.txt

# (c) Direct synthetic object factories and index-addressed field access.
#     A native that reads field slot N of a fabricated layout breaks the moment
#     the real class is loaded instead.
rg -n --hidden -g '!target/**' \
  'AnonymousObject|cratonvm/synthetic/|alloc_.*synthetic|synthetic_stub_fields' \
  classloading vm native-builtins native-collections gc \
  > target/jdk-only-audit/synthetic-object-sites.txt

# (d) Class-file ACC_SYNTHETIC. THESE ARE NOT DEFECTS. Real javac output is
#     full of ACC_SYNTHETIC members (bridge methods, captured-variable fields,
#     assertion flags). This file exists so reviewers can prove a hit is a
#     class-file attribute and not a fabricated class.
rg -n --hidden -g '!target/**' \
  'ACC_SYNTHETIC|SyntheticAttribute|is_synthetic\(|MethodAccessFlags::SYNTHETIC|ClassAccessFlags::SYNTHETIC' \
  reader classloading vm jit \
  > target/jdk-only-audit/classfile-synthetic-sites.txt

# (e) Every native lookup and dispatch entry point, plus the single decision
#     point the contract §7 routes them all through.
rg -n --hidden -g '!target/**' \
  'find_with_kind|native_methods\.find|kind_of|safe_native_call|invoke_or_native|is_native\(|resolve_dispatch|DispatchDecision' \
  vm jit jit-api native-api native-builtins \
  > target/jdk-only-audit/native-dispatch-sites.txt

# (f) Mode selectors, compatibility flags, and the hard-coded override lists
#     that decide when a native beats real bytecode.
rg -n --hidden -g '!target/**' \
  'JdkMode|CompatibilityMode|synthetic-jdk|real-jdk|jdk-only|CRATONVM_NO_STUBS|CRATONVM_REAL|prefers_real|real_protected_stub|force_native_over_real_jdk_bytecode' \
  types vm vm-cli native-api native-builtins classloading docs \
  > target/jdk-only-audit/mode-and-override-sites.txt
```

Counts are a smell test, not a result:

```bash
wc -l target/jdk-only-audit/*-sites.txt
```

### 2.1 Audit target paths

| Path | Audit objective |
|---|---|
| `types/src/compat.rs` | `CompatibilityMode`, `ExecutionPolicy` |
| `types/src/error.rs` | `VmError`, `JdkOnlyViolation` (`kind`, `summary`, `render`, `to_json`) |
| `types/src/flag_groups.rs` | `REAL`/`stubs` → `CRATONVM_NO_STUBS`; `DBG`/`dropped-stubs` → `CRATONVM_DBG_DROPPED_STUBS` |
| `vm/src/config.rs` | `JdkMode`, `CompatibilityMode`, launcher vs. embedded defaults, `validate_compatibility` |
| `vm-cli/src/main.rs` | Flag conflicts (`resolve_compatibility_mode`), `-cp`/`-classpath` normalisation, `write_jdk_only_dumps`, `finish_jdk_only`, `trace_jdk_only_violations`, `detect_jdk_feature` |
| `vm/src/vm/vm_init.rs` | Registration order and overwrite behaviour; **the census writers**: `dump_native_census_json`, `dump_class_origins_json`, `dump_jdk_only_report_json`, `render_class_origins_json`, `fold_origin_buckets`, plus `dump_missing_natives_json`, `dump_missing_natives_grouped_json`, `classify_jdk_module` |
| `native-api/src/registry.rs` | `NativeKind`, `register`, `set_category`/`with_category`, `census`, `refused_registrations`, `record_invocation`, `find_with_kind`, `CRATONVM_NO_STUBS` filtering |
| `native-builtins/src/lib.rs` | `register_essential_natives`; per-group classification correctness; duplicate registrations |
| `native-builtins-crypto/`, `native-builtins-security/`, `native-collections/`, `native-io/`, `native-awt/` | The other registrars. The report's command set only covered `native-builtins`; these crates register too. |
| `native-builtins/src/lang_system.rs` | `set_pre_exit_hook`, `native_system_exit`, `native_runtime_exit` — the `System.exit` path that bypasses the census (§3.7) |
| `classloading/src/class_origin.rs` | `ClassOrigin`, `ClassOriginEntry`, `as_str` tag vocabulary |
| `classloading/src/class_manager.rs` | `ensure_synthetic_class`, in-place stub upgrade, `is_enterprise_stub_prefix`, `is_native_backed_jdk_stub`, `synthetic_stub_fields`, `dump_class_origins`, `origin_violations` |
| `classloading/src/class.rs` | `ClassOrigin` / `is_synthetic_stub` derived mirror, `set_origin` |
| `classloading/src/loaders.rs` | Privileged-package guards, generated-accessor namespace exemptions |
| `vm/src/vm/vm_exec.rs` | `resolve_dispatch`; `invoke_or_native`; the `ThreadPoolExecutor.execute` receiver-shape special case; the `java/lang/String` forced-native list; `real_protected_stub` |
| `vm/src/runtime/interpreter.rs`, `vm/src/runtime/interpreter/invoke.rs` | `force_native_over_real_jdk_bytecode[_memoized]`, `real_protected_stub_class`, `try_stackless_invoke`, `populate_invoke_cache` |
| `jit/`, `jit-api/` | Compiled fast paths that can bypass interpreter policy |
| `scripts/jdk-only-census.sh` | The census driver (§3) |
| `tools/jdk-only-blockers/` | `blockers.py`, `selftest.py`, `status-ledger.json`, `baselines/` (§6) |
| `native-builtins/tests/stub_ratchet.rs` | The frozen `SyntheticStub` census (§4.1) |
| `native-api/tests/jdk_only_registry.rs` | Registry API contract tests (§4, §5 of the contract) |
| `classloading/tests/jdk_only_class_origin.rs` | `ClassOrigin` contract tests |
| `vm/tests/jdk_only_dispatch.rs`, `vm/tests/jdk_only_config.rs` | `resolve_dispatch` and `VmConfig` validation |
| `vm/tests/synthetic_diff.rs` | Real-path per-subsystem smoke gates |
| `.github/workflows/ci.yml` | Which gates block and which are advisory (§7) |
| `difftest/` | HotSpot oracle, mode matrix, `difftest/ledger.json`, `difftest/seeds-jdk-only` |
| `regression-suite/run.sh` | Deterministic end-to-end Java vectors; `CRATONVM_ARGS` forwarding |

---

## 3. Runtime census

### 3.1 The driver, and the defect it exists to prevent

```bash
export JAVA_HOME=/path/to/jdk25
cargo build --release -p cratonvm-cli      # or set CV=<path to cratonvm>
sh scripts/jdk-only-census.sh
```

**One VM invocation per policy; all four dumps per invocation; the policy in
every filename.** This is not a stylistic choice. The four dumps describe *one
booted VM*: a native-registry census, a class-origin census, an unresolved-native
list and a violation report are only comparable when they came from the same
process under the same compatibility policy.

> **The failure mode, stated plainly, because it will otherwise be
> re-invented.** An earlier version of this procedure ran the permissive
> (`--real-jdk`) policy and the stub-dropped (`CRATONVM_REAL=-stubs`) policy in
> separate invocations, each emitting a *different subset* of dumps, into
> filenames that did not say which policy produced them. The blocker list was
> then built from a **stub-dropped registry** paired with a
> **compatible-run class census** — two different worlds, combined into one
> answer. Nothing errors. The output is well-formed, plausible, and internally
> consistent-looking. It is simply about a VM that never existed: the registry
> says a stub is gone while the class census says the class it fabricated is
> still there, or vice versa. `tools/jdk-only-blockers/blockers.py` **cannot
> detect the mix** — the dumps carry no run identity — so the pairing is the
> caller's responsibility, and the script makes it structural rather than
> disciplinary. There is no unsuffixed dump for a consumer to pick up by
> accident.

| Policy | Launcher flag | `CompatibilityMode` | Dumps |
|---|---|---|---|
| `real` | `--real-jdk` | `Compatible` (today's VM) | `registry-real.json`, `missing-real-by-module.json`, `classes-real.json`, `report-real.json` |
| `strict` | `--jdk-only` | `JdkOnly` | `registry-strict.json`, `missing-strict-by-module.json`, `classes-strict.json`, `report-strict.json` |

Environment knobs (all optional except `JAVA_HOME`):

| Variable | Meaning |
|---|---|
| `CV` | `cratonvm` launcher. Default: `target/release/cratonvm[.exe]`, then `target/debug/…`. |
| `JAVA_HOME` | **Required.** A real runtime image — a directory with `jmods/` or `lib/modules`. `--jdk-only` refuses to start without one, so the script fails loudly (exit 3) rather than producing two empty censuses and a false all-clear. |
| `OUT` | Output directory. Default `target/jdk-only-audit`. |
| `PROBE_CP` / `PROBE_CLASS` | Census a real application instead of the built-in probe. |
| `JDK_FEATURE` | Override the feature version detected from `$JAVA_HOME/release`. |
| `PYTHON` | Interpreter for the blocker step. Default `python3`, then `python`. |
| `BLOCKERS` | `generate` (default) · `check` · `update` · `off` — see §6. |

Script exit codes:

| Code | Meaning |
|---|---|
| 0 | every artifact was produced |
| 3 | a prerequisite is missing (no `cratonvm` binary, no JDK, no `javac`, no python for `--selftest`) — nothing ran |
| 4 | the VM ran but a required dump was not written |
| 5 | blocker generation failed, or the ratchet failed |

**A non-zero *VM* exit is not a script failure.** Wave 1 expects `--jdk-only` to
fail on real workloads; the census is how that failure is measured. Only a
missing artifact fails the script. The default workload is a trivial
`JdkOnlyCensusProbe` the script compiles into `$OUT/probe/` — deliberately not
empty (it exercises boot, one allocation, a `String` path and two collections)
and deliberately small, so the artifacts stay comparable run to run instead of
becoming workload-specific.

### 3.2 The three CI aliases — all of them *strict*

The script also writes three byte-identical copies, because
`.github/workflows/ci.yml` reads them by name and the script does not own that
file:

```text
registry-no-stubs.json  <-  registry-strict.json
class-origins.json      <-  classes-strict.json
jdk-only-report.json    <-  report-strict.json
```

All three alias the **strict** set, so the alias set is coherent with itself.
Verify before trusting this claim: `alias_artifact` is called three times at the
bottom of `scripts/jdk-only-census.sh`, each time with a `*-strict.json` source.
The name `registry-no-stubs.json` is historical — it once meant a
`CRATONVM_REAL=-stubs` run, and it now means `--jdk-only`, which is a strict
superset. **New consumers should read the policy-suffixed names.** Each alias is
removed before being rewritten, so a stale copy from an earlier run can never
outlive its source.

### 3.3 The flags, and running a policy by hand

```text
--jdk-only                     Real JDK; reject compatibility stubs and fabricated classes.
--real-jdk                     Real JDK with current compatibility behaviour (default).
--synthetic-jdk                Standalone synthetic library; conflicts with both.
--jdk-only-report <FILE>       JSON violation/counter report (schema_version 1).
--dump-native-registry <FILE>  Native registry census (schema_version 2).
--dump-class-origins <FILE>    Class-origin census (schema_version 1).
--dump-missing-natives <FILE>          Unbound ACC_NATIVE calls seen this run.
--dump-missing-natives-grouped <FILE>  The same, grouped by JDK module.
--trace-jdk-only               Log violations as they are picked up (see §3.6).
--explain-jdk-only             Long-form explanation per violation; also un-redacts absolute paths.
--XX:AuditMissingNatives       Enable the missing-native audit log.
```

Both `--dump-missing-natives` flags imply `--XX:AuditMissingNatives`; the census
script passes it explicitly anyway, so the implication is never load-bearing.

Equivalent hand-run of one policy (this is exactly what `run_policy` does):

```bash
export JAVA_HOME=/path/to/jdk25
mkdir -p target/jdk-only-audit

./target/release/cratonvm \
  --java-home "$JAVA_HOME" \
  --jdk-only \
  --XX:AuditMissingNatives \
  --dump-native-registry         target/jdk-only-audit/registry-strict.json \
  --dump-missing-natives-grouped target/jdk-only-audit/missing-strict-by-module.json \
  --dump-class-origins           target/jdk-only-audit/classes-strict.json \
  --jdk-only-report              target/jdk-only-audit/report-strict.json \
  -cp vm-cli/tests/resources HelloWorld
```

Swap `--jdk-only` for `--real-jdk` and the four `-strict` suffixes for `-real`
to get the other policy. **Do not mix the two suffix sets in one command line,
and do not feed a mixed set to `blockers.py`** — that is the §3.1 defect.

`vm-cli/tests/resources` contains `HelloWorld`, `PrintArgs` and `Thrower`.

`CRATONVM_REAL=-stubs` still works as a **registry filter only**: it drops
`SyntheticStub` registrations and says nothing about class fabrication or
dispatch precedence. It expands to `CRATONVM_NO_STUBS` via
`types/src/flag_groups.rs`, and the launcher now prints a one-time note
recommending `--jdk-only` when it is set without the flag. Prefer `--jdk-only`.

### 3.4 What the dumps actually look like

There is now exactly **one writer per artifact**, all in `vm/src/vm/vm_init.rs`.
`vm-cli` used to carry independently written copies of all three; they were
deleted, because one `schema_version` with two shapes means a reader that works
against one silently mis-reads the other. `vm-cli::write_jdk_only_dumps` calls
the `vm` writers directly.

**`--dump-native-registry` — `SharedVm::dump_native_census_json`, schema 2:**

```json
{
  "schema_version": 2,
  "mode": "compatible",
  "counts": { "intrinsic": 2, "bridge": 1, "synthetic-stub": 1, "total": 4 },
  "invocations": { "intrinsic": 4301, "bridge": 1082, "synthetic-stub": 0 },
  "natives": [
    { "class": "java/lang/System", "name": "arraycopy",
      "descriptor": "([Ljava/lang/Object;I[Ljava/lang/Object;II)V",
      "kind": "intrinsic",
      "registered_by": "native-builtins/src/lib.rs:1234",
      "overwrote": "synthetic-stub",
      "invocations": 10,
      "real_declaring_method": { "loaded": true, "declared": true,
                                 "acc_native": true, "has_code": false } }
  ]
}
```

| Field | Notes |
|---|---|
| `mode` | `CompatibilityMode::as_str()` — `"compatible"` or `"jdk-only"`. This is how a dump proves which policy produced it; do not strip it. |
| `counts` | **Registrations**, one row per `register()` call including superseded ones. Numerically identical to schema 1, which is why the stub ratchet still reads it. |
| `invocations` (top level) | Per-kind **dispatch** totals. A different measurement from `counts`; the two have been confused before. |
| `kind` | `"intrinsic"`, `"bridge"`, `"synthetic-stub"`. |
| `registered_by` | `#[track_caller]` `Location`. **Redacted unless `--explain-jdk-only`**, so committed baselines do not carry the builder's home directory. `null` when not recorded. |
| `overwrote` | `NativeKind` of the entry this registration displaced, or `null`. This is how you find a `Bridge` silently downgraded by a later default-category `register()`. |
| `invocations` (per row) | Per-slot relaxed counter. A `synthetic-stub` with `0` is dead-registration cleanup; a nonzero one is live behavioural debt. **It is not a ratchet input** — see §6. |
| `real_declaring_method` | See §3.5. |

Rows sort by `(class, name, descriptor)` with a **stable** sort, so duplicate
triples stay in registration order — the overwrite chronology — and the file is
byte-stable across machines.

**`--dump-class-origins` — `SharedVm::dump_class_origins_json`, schema 1:**

```json
{
  "schema_version": 1,
  "counts": { "boot-image": 312, "compatibility-stub": 0, "...": 0, "total": 343 },
  "classes": [
    { "name": "java/lang/String", "origin": "boot-image", "reason": null,
      "requested_by": null, "real_bytes_found": true, "loader_id": 0 }
  ]
}
```

`counts` is **seeded to zero from the full `ClassOrigin::as_str()` vocabulary**,
so `"compatibility-stub": 0` is *stated* rather than implied by an absent key —
the one assertion a strict-mode reader most wants, and the one an omission would
silently fake. Rows sort by `(name, loader_id, origin)`: a name legitimately
appears once per defining loader, and twice with different origins during an
in-place stub-upgrade window. `name`, `reason` and `requested_by` are redacted
unless `--explain-jdk-only`; `origin` is a closed tag vocabulary and is never
redacted. Emitted in **both** modes — under `Compatible` origins are recorded
but nothing is refused, which is what makes the census a usable baseline before
enforcement lands.

**`--jdk-only-report` — `SharedVm::dump_jdk_only_report_json`, schema 1:**

```json
{
  "schema_version": 1,
  "mode": "jdk-only",
  "jdk_feature": 25,
  "violations": [],
  "counts": {
    "boot_image_classes": 312, "application_classes": 18,
    "generated_classes": 4,   "compatibility_classes": 0,
    "bridge_invocations": 1082, "intrinsic_invocations": 4301,
    "synthetic_stub_invocations": 0
  }
}
```

`violations` unions the registry's refused registrations with the class
manager's origin violations, each rendered by `JdkOnlyViolation::to_json()`, and
is **sorted by `(kind, summary)`** — origin violations are recorded from
arbitrary application threads, so recording order is nondeterministic and
sorting is what makes two runs byte-identical. (The native census makes the
opposite choice for the opposite reason: registration order *is* the fact.)

The four class counters are a **partition** of the ten `ClassOrigin` tags
(`fold_origin_buckets`), so their sum is the row count and no class can go
missing between this summary and the class-origin census. Read the partition
before quoting a number: `application_classes` folds in `user-defined`, and
`generated_classes` folds in `vm-internal` **and** any origin tag the fold does
not recognise. `counts` carries exactly the seven keys the contract spells and
no more.

The report is a **separate, smaller** artifact from the native census. Do not
conflate the two.

### 3.5 `real_declaring_method` — verify before you key off it

The surviving writer (`dump_native_census_json`) emits a **four-key object**:

```json
"real_declaring_method": { "loaded": true, "declared": true,
                           "acc_native": true, "has_code": false }
```

- `loaded` — the declaring class was already loaded this run. The lookup is
  `get_loaded_class_id`, a read through `&self`, **not** an initiating load, so
  the census does not perturb what it measures and `false` is a real answer
  ("this run never loaded it") rather than a probe declined. Caveat: the lookup
  is requester-less, so for a name no built-in loader has defined it can answer
  from a lone user-defined loader's copy.
- `declared` — the real class declares that `(name, descriptor)`.
- `acc_native` — the real method is `ACC_NATIVE`.
- `has_code` — derived from the access flags (JVMS §4.6: `Code` is present iff
  the method is neither `native` nor `abstract`), **not** from
  `ClassFileMethod::code()`, which returns `None` for a not-yet-force-decoded
  lazy attribute — a decode-state artefact, not a fact about the class.

> **This field is mid-edit; treat the inner key names as unstable.** Three
> spellings exist in the tree as of this writing: the contract's §9 example
> (`{present, acc_native, has_code}`), the surviving writer's four keys above,
> and `tools/jdk-only-blockers/selftest.py`'s fixture
> (`{class_loaded, class_is_compatibility_stub, declared, has_code, acc_native}`).
> Two divergent census writers were being unified while this section was
> written. `blockers.py` copies the object through **verbatim**
> (`row.get('real_declaring_method')`) and never reads inside it, so the blocker
> artifacts are unaffected either way — but a bespoke consumer that keys off
> `present` or `loaded` will break. Re-read `dump_native_census_json` before
> writing one, and treat the contract's example as the *intent*, not the wire
> format.

The classification argument in §5 turns on this triple regardless of spelling:
declaring class absent ⇒ the native stands in for a class that does not exist;
present and `ACC_NATIVE` ⇒ legitimate bridge candidate; present with concrete
bytecode ⇒ the native shadows real code.

### 3.6 `--trace-jdk-only` is a poll, not a live trace

The two recording sites (`ClassManager::origin_violations`,
`NativeMethodRegistry::refused_registrations`) are append-only vectors with no
sink installed. The launcher drains them at the only two points it holds the VM:

1. immediately after `Vm::new` — which is when registration refusals actually
   happen, so draining only at shutdown would report them minutes late, after
   the failure they caused; and
2. again at shutdown.

**Consequence for the reader:** a class-origin violation recorded mid-run does
not appear on stderr when it occurs. It surfaces in the shutdown drain, out of
order with respect to the program's own output, and *after* whatever failure it
caused. Do not read the absence of a trace line as the absence of a violation at
that moment, and do not correlate trace-line position with program position. The
final `--jdk-only-report` is the authority on *what* was recorded; the trace is
only a convenience for seeing it without opening the file. A genuinely live
trace needs a VM-scoped sink at the recording sites — a wave-2 change to
`classloading` and `native-api`, not something the launcher can fake.

### 3.7 Limitation: `System.exit(N)` bypasses the census entirely

`write_jdk_only_dumps` is reached from four call sites in `vm-cli/src/main.rs` —
three failing paths (main-class load failure, agent `premain` failure, a panic
escaping the main invocation) and the normal end-of-run path — and writes at
most once per process, first caller wins, so a strict-mode failure still leaves
a categorisable census.

**None of that survives `System.exit`.** `native_system_exit` and
`native_runtime_exit` (`native-builtins/src/lang_system.rs`) call
`std::process::exit`, which does not unwind: no `Drop`, no return through
`run()`, no `finish_jdk_only`. The pre-exit hook installed at the top of `run()`
covers staged-archive cleanup, JFR `dumponexit`, `CRATONVM_DBG_EXIT` and the JIT
method stats — it does **not** write the JDK-only dumps.

So: **a program that calls `System.exit` produces no census at all**, and the
script reports the dumps as missing (exit 4) or, for the optional ones, as "not
written". A reader who does not know this will see an absent or short dump and
conclude the run was clean. It was not measured.

Practical consequences:

- The built-in probe returns from `main` normally, on purpose. Keep it that way.
- Before pointing `PROBE_CP`/`PROBE_CLASS` at a real application, check whether
  it exits via `System.exit` — many CLI applications and most test harnesses do.
  If it does, the census for that run is not evidence of anything.
- If you must census such an application, wrap the entry point in a main class
  that calls the real `main` and returns, or accept that only the pre-`exit`
  portion was observed and say so on the artifact.

This is a limitation of the audit procedure, not a bug you can work around by
re-running.

### 3.8 Instrumentation completeness

A census is only as good as the paths it observes. It must record:

- every registration **and every overwrite**, not just the surviving entry;
- the final classification;
- every invocation through the interpreter, the JIT helper path, JNI,
  reflection, method handles, and the direct fast paths;
- whether a concrete real method body was available at dispatch time;
- every `ensure_synthetic_class` / enterprise-fallback call;
- the allocation factory and expected field layout for fabricated objects;
- the initiating and defining loader;
- the first call site, plus a bounded count of subsequent uses;
- the JDK feature version and the module owning the class;
- whether the event **would be** rejected under `JdkOnly` (recorded even in
  `Compatible` mode — this is what makes the census meaningful before
  enforcement lands).

Known gaps against that list are tracked in
[`known-issues/jdk-only/`](known-issues/jdk-only/); read them before quoting a
count as complete.

---

## 4. Coverage runs

```bash
# The frozen synthetic-stub census. Prints the live count before asserting.
cargo test -p cratonvm-native-builtins --test stub_ratchet -- --nocapture

# The cross-crate strict contracts (the same four the CI jdk-only job runs).
cargo test -p cratonvm-native-api      --test jdk_only_registry
cargo test -p cratonvm-classloading    --test jdk_only_class_origin
cargo test -p cratonvm-vm              --test jdk_only_dispatch
cargo test -p cratonvm-vm              --test jdk_only_config

# Real-path per-subsystem smoke gates.
cargo test -p cratonvm-vm --test synthetic_diff -- --nocapture

# Deterministic end-to-end Java vectors, CratonVM vs HotSpot.
# NOTE: regression-suite is a shell harness, not a cargo crate. CRATONVM_ARGS is
# forwarded to every CratonVM invocation (and never to the HotSpot oracle);
# naming --jdk-only in it also implies the RJdk* JDK-only corpus.
CV="$PWD/target/release/cratonvm" JDK="$JAVA_HOME" \
  CRATONVM_ARGS="--jdk-only" bash regression-suite/run.sh

# HotSpot differential gate against the committed ledger.
cargo run -p cratonvm-difftest --bin cratonvm-difftest -- \
  gate --jdk "$JAVA_HOME" --corpus difftest/seeds

# The strict corpus, under the policy modes.
cargo run -p cratonvm-difftest --bin cratonvm-difftest -- \
  gate --corpus difftest/seeds-jdk-only --modes jdk-only-jit,jdk-only-nojit
```

`difftest` mode labels (`difftest/src/runner.rs`, `Mode::label`) are now
`jit-on`, `nojit`, `no-intrinsics`, `moving-gc`, `low-jit-threshold`,
`jdk-only-jit`, `jdk-only-nojit`, `real-compatible-jit`,
`real-compatible-nojit`. `--modes` still defaults to `jit-on,nojit`, and the
historical five still pass no launcher flags so their committed baselines stay
valid. The four policy modes are the ones that pass `--jdk-only` / `--real-jdk`
and turn the census dumps on (`Mode::collects_census`); ledger rows are keyed by
`(class, jdk_profile)`, where `real-compatible-*` reports `"compatible"` — it
differs from `jit-on`/`nojit` in *observability*, not policy.

### 4.1 Reading the ratchet

`native-builtins/tests/stub_ratchet.rs` builds a fresh `NativeMethodRegistry`,
calls `register_essential_natives`, and counts rows from `dump_registrations()`.
It asserts:

- `synthetic <= BASELINE_SYNTHETIC_STUBS` — currently **157**, with `SLACK = 0`;
- `total >= MIN_TOTAL_REGISTRATIONS` — **8,000**, a vacuity floor;
- for the strict registry, `strict_total >= STRICT_MIN_TOTAL_REGISTRATIONS`
  (**7,500**), so "zero synthetic stubs" cannot be achieved by the registry
  collapsing.

`strict_mode_refuses_nothing` is the end-state gate and is deliberately
`#[ignore]`d until `BASELINE_SYNTHETIC_STUBS` reaches 0.

> **Do not quote "~9,320 total registrations."** The research report described
> the baseline as "157 `SyntheticStub` entries among roughly 9,320
> registrations." Only the **157 is asserted, and it is asserted exactly**. The
> denominator is not asserted at all — the floor is a loose 8,000 precisely
> because the 9,320 came from a historical argument that was never checked.
> `tools/jdk-only-blockers/blockers.py` copies `registry_totals` verbatim from
> the census's own `counts` and re-derives nothing from it, for the same reason.
> If you need a total, read `.counts.total` from a fresh
> `--dump-native-registry` and say which run it came from.

The ratchet counts the **default build**. The `synthetic-jdk` override block is
feature-gated and deliberately not counted.

---

## 5. Classification rubric

Every census row resolves to exactly one disposition. Answer in order; stop at
the first rule that fires. Feed each row through
[`jdk-only-native-review.md`](jdk-only-native-review.md) — that document is the
promotion checklist this table is the index to.

| # | Condition | Disposition |
|---|---|---|
| 1 | Real declaring class/method absent from the boot image, and the registration supplies compatibility behaviour | **CompatibilityShim** — forbidden under `JdkOnly` |
| 2 | Real method is `ACC_NATIVE` **and** the callback is a complete VM/OS implementation | **Bridge** |
| 3 | Real method has concrete bytecode **and** the callback is proven observationally equivalent (differential-tested) | **Intrinsic** |
| 4 | Real method has concrete bytecode **and** the callback is an incomplete replacement | **Delete from the strict path** — real bytecode wins |
| 5 | `invocations == 0` across the whole corpus, and no test invokes it | **Delete** — dead registration |
| 6 | The entry exists to stand in for a runtime-generated artifact (lambda, proxy, accessor) | **Move to the generated-class service** — it is not a native at all |

> **A `synthetic-stub` row is a classification *request*, not a confirmed
> defect.** Because the kind is ambient and defaults to `SyntheticStub` (see
> *Why a source grep is not the source of truth*, above), a
> `register()` call outside a `with_category(...)` block is tagged
> `synthetic-stub` by omission — so some rows are genuinely mis-tagged permanent
> bridges that need re-tagging, not removal. The blocker artifacts say this
> explicitly with `"requires_classification": true` and never assert the row is
> a defect. Rule 1 above is the test that separates the two; run it before
> filing anything. `docs/jdk-only-ambient-category-audit.md` inventories the
> ambient-category sites themselves.

For classes:

| Condition | `ClassOrigin` | `JdkOnly` |
|---|---|---|
| Loaded from the JDK runtime image | `BootImage` | allowed |
| Loaded from `-cp` / `-jar` | `ApplicationClassPath` | allowed |
| Defined by a user loader from real bytes | `UserDefined` | allowed |
| VM-created array class | `VmArray` | allowed **in both modes** |
| `Lookup.defineHiddenClass` | `HiddenClass` | allowed |
| Lambda / proxy / reflection accessor from generated bytes | `GeneratedLambda` / `GeneratedProxy` / `ReflectionAccessor` | allowed |
| VM bookkeeping class | `VmInternal` | allowed |
| Fabricated, no real bytes | `CompatibilityStub` | **forbidden** |

The class-file `ACC_SYNTHETIC` flag and the `Synthetic` attribute are **never** a
defect. `target/jdk-only-audit/classfile-synthetic-sites.txt` exists to make that
argument quickly, not to enumerate problems.

### 5.1 Worksheet generation

With `jq`:

```bash
jq -r '
  .natives[]
  | select(.kind == "synthetic-stub")
  | [.class, .name, .descriptor, (.registered_by // "-"), (.invocations // 0)]
  | @tsv
' target/jdk-only-audit/registry-real.json \
  > target/jdk-only-audit/stub-review.tsv

wc -l target/jdk-only-audit/stub-review.tsv   # expect ~157 today
```

`jq` is not a prerequisite of this repository, but `python3` is (the blocker
tooling needs it), so the portable form is:

```bash
python3 - target/jdk-only-audit/registry-real.json <<'PY' \
  > target/jdk-only-audit/stub-review.tsv
import json, sys
for r in json.load(open(sys.argv[1], encoding='utf-8'))['natives']:
    if r.get('kind') == 'synthetic-stub':
        print('\t'.join([r['class'], r['name'], r['descriptor'],
                         r.get('registered_by') or '-',
                         str(r.get('invocations', 0))]))
PY
```

Use `registry-real.json` — the *permissive* registry — deliberately: strict mode
refuses these registrations, so `registry-strict.json` is where you check that
the count reached zero, not where you find the work list.

---

## 6. Per-JDK blocker artifacts

Two machine-generated files per policy, keyed by JDK feature version:

```text
target/jdk-only-audit/jdk-<feature>-missing-natives.json           # policy: real
target/jdk-only-audit/jdk-<feature>-synthetic-dependencies.json    # policy: real
target/jdk-only-audit/strict/jdk-<feature>-missing-natives.json        # policy: strict
target/jdk-only-audit/strict/jdk-<feature>-synthetic-dependencies.json # policy: strict
```

Generated by `tools/jdk-only-blockers/blockers.py`, driven by the census script.
No static review can produce an exhaustive missing-method list across "any JDK":
native surfaces and class-library internals vary by feature version, so **the
list is generated per image and is only valid for that image.** A file generated
against JDK 25 says nothing about JDK 21.

```bash
sh scripts/jdk-only-census.sh                    # BLOCKERS=generate (default)
BLOCKERS=check  sh scripts/jdk-only-census.sh    # generate + ratchet the `real` pair
BLOCKERS=update sh scripts/jdk-only-census.sh    # generate + refresh the `real` baseline
BLOCKERS=off    sh scripts/jdk-only-census.sh    # dumps only
```

The feature version is **passed explicitly** (`--jdk-feature`), read from
`$JAVA_HOME/release`'s `JAVA_VERSION=` by the script's `detect_jdk_feature`
(which mirrors the launcher's own), falling back to `java -version`, and
overridable with `JDK_FEATURE`. It is never inferred from a report, because a
strict run that dies during boot may never write one — and the version is part
of the file name, so an artifact keyed to the wrong image is worse than no
artifact. If it cannot be determined, the script exits 5 and tells you to set
`JDK_FEATURE` or `BLOCKERS=off`.

`blockers.py` exits 1 (ratchet grew), 2 (usage/configuration, including an
off-vocabulary status ledger entry, an undeterminable feature version, or
`--check` with no committed baseline) or 3 (a partial result where a complete
one was required). The census script **collapses all three onto its own exit 5**
and names which one fired, so a caller never has to guess which tool produced a
bare status.

### 6.1 Only the `real` pair is ratcheted — deliberately

`tools/jdk-only-blockers/baselines/` holds **one pair per JDK feature version**.
It is not keyed by policy. Ratcheting a second policy against it would compare a
strict run to a permissive baseline — the same cross-world comparison §3.1
exists to prevent, re-created one level up. So the strict pair is generated for
inspection and never checked.

The `real` pair is also the more useful measure today: wave 1 is measurement,
not deletion, so the `Compatible` run's census is exactly the distance
`--jdk-only` still has to travel.

**`baselines/` is empty as of this writing**, so `BLOCKERS=check` correctly
refuses (`blockers.py` exit 2 → script exit 5): there is nothing to ratchet
against. Commit `baselines/jdk-<feature>-*.json` first, via `BLOCKERS=update`
(or `blockers.py … --update-baseline`) followed by
`git add tools/jdk-only-blockers/baselines/`. Baselines are byte-stable: fixed
sort order, no timestamps, no absolute paths, LF newlines — a diff is always a
real change. A baseline is **refused from a partial result** unless
`--allow-partial` is passed, because freezing a partial run bakes in a
false-clean ratchet.

### 6.2 An uninvoked stub is still a stub

`--check` compares the **open** key set against the baseline and fails if it
grew. The keys are `category|class.name(descriptor)` for natives and
`category|class-name` for classes. **`invocations` is deliberately not part of
the key.** A brand-new registration with `invocations: 0` fails the ratchet
exactly like a hot one: it is still a registered stub, it will still be
dispatched by some workload this probe did not run, and "nothing called it
today" is not a property of the code. An entry that regresses from a closed
status back to `open` also fails. Closed blockers that *disappear* are reported
but do not fail — refresh the baseline to lock the improvement in.

Closure is a human judgement recorded in
`tools/jdk-only-blockers/status-ledger.json`, joined onto the machine-observed
rows. The vocabulary is closed: `bridge`, `intrinsic`, `real-bytecode`,
`generated-class`, `out-of-scope`. Anything else is `open`; there is no
`wontfix` and no `deferred`, and an off-vocabulary status is a hard error so a
typo can never silently close a blocker.

### 6.3 Self-test

```bash
sh scripts/jdk-only-census.sh --selftest
# or, equivalently:
python tools/jdk-only-blockers/selftest.py
```

Hermetic: no `cratonvm` binary, no `JAVA_HOME`, no network, standard library
only. `--selftest` is handled **before every prerequisite check** in the census
script, so it is safe in a lint or docs job that provisions nothing. It covers
generation, determinism (byte-identical reruns, no CRLF, no absolute paths), the
ratchet (including that a never-invoked stub fails it and an extra allowed
generated class does not), the status ledger, and every degraded-input path.

### 6.4 Partial results are never a clean result

A missing or malformed dump produces a **partial** result: `"partial": true`,
an `inputs` map recording `present`/`missing`/`malformed`/`not-provided` per
role, an explicit `notes` sentence saying the file is *not* evidence of a clean
run, and a stderr warning regardless of `--quiet`. `--check` and
`--update-baseline` refuse. If no input can be read at all, **nothing is
written** and the tool exits 3 — an empty blocker file on disk would read as
"nothing is broken", which is the one outcome that must never be possible.

An unrecognised `ClassOrigin` tag is named in `notes` and marks the run partial;
it is never silently treated as allowed. New origins must be added to
`ALLOWED_GENERATED_ORIGINS` or `REAL_BYTES_ORIGINS` in `blockers.py` by a human.

---

## 7. CI posture

- **JDK 25 is pinned everywhere.** Every `actions/setup-java` step in
  `.github/workflows/ci.yml`, `coverage.yml` and `performance.yml` uses
  `java-version: '25'`, including `real-path-coverage` and `difftest-gate`. The
  research report's claim that real-path CI uses JDK 21 is wrong.
- **The one exception is the advisory `jdk-only` job**, which runs a
  `{ubuntu-latest, windows-latest} × {21, 25}` matrix under
  `continue-on-error: true`. Because it is advisory, **JDK 21 has no blocking
  coverage anywhere today**, and JDK 17 appears in no workflow at all. Any
  17/21/25 matrix — including the "JDK 21 and 25, with 17 nightly" one the
  research report proposes — is a **proposal, not current coverage.** Since the
  blocker artifacts are only valid for the image that produced them, each matrix
  leg needs its own committed baseline pair before it can be ratcheted.
- The `jdk-only` job is advisory on purpose, for two independent reasons stated
  in the workflow: wave 1 is measurement, so a `--jdk-only` run is *expected* to
  fail on real workloads while 157 stubs remain; and the contract makes the
  zero-stub census blocking only once the strict corpus shows no new HotSpot
  divergence. Promote it by deleting `continue-on-error` together with
  un-ignoring `strict_mode_refuses_nothing`.
- **The neighbouring gates are blocking.** The `Synthetic-stub ratchet` step
  lives in `build-and-test`, which provisions JDK 25 and carries no
  `continue-on-error`. `difftest-gate` is likewise blocking. The research
  report's claim that the stub census is advisory "because that job does not
  provision a JDK" is wrong on both halves.
- Turning the census step into a ratchet is one environment variable —
  `BLOCKERS: check` — and needs no new step. Keep `--allow-partial` out of CI: a
  partial run in a gate is a green light for a dump that did not get written.
  Upload the artifacts with `if: always()` so a failure ships its own evidence.

---

## 8. Artifact inventory

A complete `sh scripts/jdk-only-census.sh` run, plus §1 and §2, leaves:

```text
target/jdk-only-audit.commit
target/jdk-only-audit.dirty
target/jdk-only-audit/cargo-metadata.json
target/jdk-only-audit/files.txt

target/jdk-only-audit/native-kind-sites.txt
target/jdk-only-audit/synthetic-class-sites.txt
target/jdk-only-audit/synthetic-object-sites.txt
target/jdk-only-audit/classfile-synthetic-sites.txt        # reference, not defects
target/jdk-only-audit/native-dispatch-sites.txt
target/jdk-only-audit/mode-and-override-sites.txt

target/jdk-only-audit/probe/                               # the generated workload
target/jdk-only-audit/real.stdout.txt
target/jdk-only-audit/real.stderr.txt
target/jdk-only-audit/strict.stdout.txt
target/jdk-only-audit/strict.stderr.txt

target/jdk-only-audit/registry-real.json                   # policy: real
target/jdk-only-audit/missing-real-by-module.json
target/jdk-only-audit/classes-real.json
target/jdk-only-audit/report-real.json

target/jdk-only-audit/registry-strict.json                 # policy: strict
target/jdk-only-audit/missing-strict-by-module.json
target/jdk-only-audit/classes-strict.json
target/jdk-only-audit/report-strict.json

target/jdk-only-audit/registry-no-stubs.json               # alias of registry-strict
target/jdk-only-audit/class-origins.json                   # alias of classes-strict
target/jdk-only-audit/jdk-only-report.json                 # alias of report-strict

target/jdk-only-audit/jdk-<feature>-missing-natives.json           # from the real set
target/jdk-only-audit/jdk-<feature>-synthetic-dependencies.json    # from the real set
target/jdk-only-audit/strict/jdk-<feature>-missing-natives.json
target/jdk-only-audit/strict/jdk-<feature>-synthetic-dependencies.json

target/jdk-only-audit/stub-review.tsv                      # §5.1, generated by hand
```

Required by the script (absence is exit 4): `registry-real.json`,
`missing-real-by-module.json`, `registry-strict.json`, `classes-strict.json`,
and the `registry-no-stubs.json` / `class-origins.json` aliases. The rest are
reported as "not written" rather than failing, because a strict run that dies
during boot legitimately produces less — and the derived artifacts mark
themselves `partial` rather than pretending the missing census was empty.

`target/` is not committed. Upload these as CI artifacts on every strict-job
failure, and attach the relevant ones to any issue filed via
[`.github/ISSUE_TEMPLATE/jdk-only.yml`](../.github/ISSUE_TEMPLATE/jdk-only.yml).

---

## 9. Verification status of the commands in this document

Run against this checkout while writing:

| Command | Status |
|---|---|
| `sh -n scripts/jdk-only-census.sh` | **verified** — parses clean |
| `sh scripts/jdk-only-census.sh --selftest` | **verified** — all checks passed, no VM/JDK needed |
| `python tools/jdk-only-blockers/selftest.py` | **verified** — all checks passed |
| `python tools/jdk-only-blockers/blockers.py --help` | **verified** — every flag cited above exists |
| The six `rg` commands in §2 | **verified** — all six run and produce non-empty output |
| Everything requiring a built `cratonvm` and a JDK image — `sh scripts/jdk-only-census.sh` proper, the hand-run in §3.3, the `cargo test` / `difftest` / `regression-suite` lines in §4 | **unverified here.** They are transcribed from the tooling and the workflow rather than executed; this session was barred from running cargo. Re-run them before citing their output. |
| `jq` in §5.1 | **unverified** — `jq` is not present on every developer machine (it was absent here). The `python3` form below it is the portable one. |
