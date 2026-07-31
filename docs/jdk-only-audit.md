# JDK-only mode — reproducible audit

| | |
|---|---|
| **Status** | Wave 1 (measurement). The static half runs today; the runtime-census half runs partially today and completes when the wave-1 dumps land. |
| **Normative source** | [`feature-designs/jdk-only-mode.md`](feature-designs/jdk-only-mode.md) — the interface contract. This document does not define semantics; it defines how to *measure* them. |
| **Companions** | [`jdk-only-runtime-services.md`](jdk-only-runtime-services.md) (blocker inventory) · [`jdk-only-native-review.md`](jdk-only-native-review.md) (promotion checklist) · [`benchmarks/jdk-only.md`](benchmarks/jdk-only.md) |

The purpose of this audit is to produce a **runtime-observed** inventory of every
compatibility substitution the VM performs, with provenance. A source grep alone
is not the source of truth, for three reasons that are properties of this
repository and not of the report that proposed the work:

1. **Registrations overwrite one another.** `NativeMethodRegistry::register` is
   called thousands of times and later entries replace earlier ones for the same
   `(class, name, descriptor)` key. `vm/src/vm/vm_init.rs` deliberately
   re-registers some implementations *after* the broad collection passes.
2. **The native kind is ambient, not per-call.** `register()` takes no kind
   argument. The kind comes from the registry's current category, set by
   `NativeMethodRegistry::set_category` / `with_category`
   (`native-api/src/registry.rs`), and **defaults to `SyntheticStub`**. A
   `register()` call that is not inside a `with_category(...)` block is a
   `SyntheticStub` by omission, which a grep for `NativeKind::SyntheticStub`
   will never show.
3. **Some classes are fabricated at runtime.** `ClassManager::ensure_synthetic_class`
   and the enterprise-prefix fallback in `ClassManager::load_class`
   (`classloading/src/class_manager.rs`) create classes that exist in no source
   file.

> **Citation convention.** This document cites **file + symbol**, not
> `file:line`. Wave-1 implementation is in flight across nine agents in the same
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
> shared checkout whose `dev` branch moves under concurrent sessions, and
> `git checkout`/`git pull` will discard or relocate another agent's work.
> Record the commit; do not move to it.

---

## 2. Static inventory

Run each category separately. They answer different questions and must not be
merged into one file.

```bash
mkdir -p target/jdk-only-audit

# (a) Native classification and the ambient-category sites that decide it.
#     `set_category` / `with_category` matter as much as the enum literals:
#     the registry's default category is SyntheticStub.
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

# (e) Every native lookup and dispatch entry point.
rg -n --hidden -g '!target/**' \
  'find_with_kind|native_methods\.find|kind_of|safe_native_call|invoke_or_native|is_native\(' \
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

Every path below was verified to exist in this repository at the time of
writing.

| Path | Audit objective |
|---|---|
| `vm/src/config.rs` | `JdkMode`, `CompatibilityMode`, launcher vs. embedded defaults, `validate_compatibility` |
| `vm-cli/src/main.rs` | Flag conflicts, `-cp`/`-classpath` normalisation, mode propagation, dump wiring |
| `vm/src/vm/vm_init.rs` | Registration order and overwrite behaviour; `register_essential_natives` call site; `dump_native_registry_json`, `dump_missing_natives_json`, `dump_missing_natives_grouped_json`, `classify_missing_natives_by_module` |
| `native-api/src/registry.rs` | `NativeKind`, `register`, `set_category`/`with_category`, `dump_registrations`, `find_with_kind`, `CRATONVM_NO_STUBS` filtering |
| `native-builtins/src/lib.rs` | `register_essential_natives`; per-group classification correctness; duplicate registrations |
| `native-builtins-crypto/`, `native-builtins-security/`, `native-collections/`, `native-io/`, `native-awt/` | The other registrars. The report's command set only covered `native-builtins`; these crates register too. |
| `classloading/src/class_manager.rs` | `ensure_synthetic_class`, in-place stub upgrade, `is_enterprise_stub_prefix`, `is_native_backed_jdk_stub`, `synthetic_stub_fields` |
| `classloading/src/class.rs` | `ClassOrigin` / `is_synthetic_stub` derived mirror |
| `classloading/src/loaders.rs` | Privileged-package guards, generated-accessor namespace exemptions |
| `vm/src/vm/vm_exec.rs` | `invoke_or_native`; the `ThreadPoolExecutor.execute` receiver-shape special case; the `java/lang/String` forced-native list; `real_protected_stub` |
| `vm/src/runtime/interpreter.rs`, `vm/src/runtime/interpreter/invoke.rs` | `force_native_over_real_jdk_bytecode[_memoized]`, `real_protected_stub_class`, `try_stackless_invoke`, `populate_invoke_cache` |
| `jit/`, `jit-api/` | Compiled fast paths that can bypass interpreter policy |
| `types/src/flag_groups.rs` | `REAL`/`stubs` → `CRATONVM_NO_STUBS`; `DBG`/`dropped-stubs` → `CRATONVM_DBG_DROPPED_STUBS` |
| `types/src/error.rs` | `VmError`, `JdkOnlyViolation` |
| `vm/tests/synthetic_diff.rs` | Real-path per-subsystem smoke gates |
| `native-builtins/tests/stub_ratchet.rs` | The frozen `SyntheticStub` census |
| `native-api/tests/registry_contracts.rs` | Registry API contract tests |
| `.github/workflows/ci.yml` | Which gates block and which are advisory |
| `difftest/` | HotSpot oracle, mode matrix, `difftest/ledger.json` |
| `regression-suite/run.sh` | Deterministic end-to-end Java vectors |

---

## 3. Runtime census

### 3.1 What exists today

These three dump flags are **already implemented** in `vm-cli/src/main.rs` and
work on any current build:

```text
--dump-native-registry <FILE>          final native classification
--dump-missing-natives <FILE>          unbound ACC_NATIVE calls seen this run
--dump-missing-natives-grouped <FILE>  the same, grouped by JDK module
```

```bash
export JAVA_HOME=/path/to/jdk25

cargo build -p cratonvm-cli

# Baseline: current default behaviour (real JDK + compatibility overlays).
cargo run -p cratonvm-cli --bin cratonvm -- \
  --real-jdk \
  --java-home "$JAVA_HOME" \
  --dump-native-registry           target/jdk-only-audit/registry-real.json \
  --dump-missing-natives           target/jdk-only-audit/missing-real.json \
  --dump-missing-natives-grouped   target/jdk-only-audit/missing-real-by-module.json \
  -cp vm-cli/tests/resources HelloWorld

# The existing low-level approximation: drop registered SyntheticStub natives.
# This filters the registry ONLY. It does not stop class fabrication, does not
# change dispatch precedence, and does not reclassify anything.
CRATONVM_REAL=-stubs \
CRATONVM_DBG=dropped-stubs \
cargo run -p cratonvm-cli --bin cratonvm -- \
  --real-jdk \
  --java-home "$JAVA_HOME" \
  --dump-native-registry         target/jdk-only-audit/registry-no-stubs.json \
  --dump-missing-natives-grouped target/jdk-only-audit/missing-no-stubs.json \
  -cp vm-cli/tests/resources HelloWorld
```

`CRATONVM_REAL=-stubs` expands to `CRATONVM_NO_STUBS` and `CRATONVM_DBG=dropped-stubs`
to `CRATONVM_DBG_DROPPED_STUBS`; both mappings are declared in
`types/src/flag_groups.rs`.

`vm-cli/tests/resources` contains `HelloWorld`, `PrintArgs` and `Thrower`.

### 3.2 What wave 1 adds

Per contract §9, `vm-cli` gains:

```text
--jdk-only                    Real JDK, reject compatibility stubs and fabricated classes.
--jdk-only-report <FILE>      JSON violation/counter report (schema_version 1).
--dump-class-origins <FILE>   Class-origin census.
--trace-jdk-only              Log every violation as it happens.
--explain-jdk-only            Long-form explanation per violation; also un-redacts absolute paths.
```

Full diagnostic capture once those land:

```bash
cargo run -p cratonvm-cli --bin cratonvm -- \
  --jdk-only \
  --java-home "$JAVA_HOME" \
  --jdk-only-report              target/jdk-only-audit/report-strict.json \
  --dump-native-registry         target/jdk-only-audit/registry-strict.json \
  --dump-class-origins           target/jdk-only-audit/classes-strict.json \
  --dump-missing-natives-grouped target/jdk-only-audit/missing-strict.json \
  -cp vm-cli/tests/resources HelloWorld
```

Wave 1 is diagnostic-first: under `--jdk-only` only class fabrication (contract
§5) and stub *registration* (contract §4) enforce. Everything else records and
counts. A clean `--jdk-only` run in wave 1 therefore does **not** mean the
program executed only real JDK implementations.

### 3.3 Current registry-dump shape

`SharedVm::dump_native_registry_json` (`vm/src/vm/vm_init.rs`) writes exactly
this today. There is **no `schema_version` key**:

```json
{
  "counts": {
    "intrinsic": 0,
    "bridge": 0,
    "synthetic-stub": 0,
    "total": 0
  },
  "natives": [
    { "class": "java/lang/String", "name": "length", "descriptor": "()I", "kind": "intrinsic" }
  ]
}
```

Rows are sorted by `(class, name, descriptor)`. Treat the above as the implicit
**schema 1**. `jq '.counts["synthetic-stub"]'` works against it today.

### 3.4 Schema-v2 census

Contract §9 bumps the native census to `"schema_version": 2`, adding
`registered_by`, `overwrote`, `invocations` and `real_declaring_method` per
entry. The full audit artifact, combining the native and class censuses:

```json
{
  "schema_version": 2,
  "vm_commit": "<git-sha>",
  "jdk": {
    "home": "<redacted unless --explain-jdk-only>",
    "feature_version": 25,
    "runtime_version": "<banner>",
    "modules_hash": "<optional>"
  },
  "mode": "compatible",
  "natives": [
    {
      "class": "java/lang/String",
      "name": "length",
      "descriptor": "()I",
      "kind": "intrinsic",
      "registered_by": "native-builtins/src/lib.rs:1234",
      "overwrote": "synthetic-stub",
      "invocations": 123,
      "real_declaring_method": {
        "present": true,
        "acc_native": false,
        "has_code": true
      }
    }
  ],
  "classes": [
    {
      "name": "java/util/function/Function$Identity",
      "origin": "compatibility-stub",
      "reason": "Function.identity() stand-in",
      "requested_by": "java/util/function/Function.identity()Ljava/util/function/Function;",
      "real_bytes_found": false,
      "loader_id": 0
    }
  ]
}
```

Field notes, all normative against the contract:

| Field | Source | Notes |
|---|---|---|
| `mode` | `CompatibilityMode::as_str()` | `"compatible"` or `"jdk-only"` — machine-greppable spellings fixed by contract §3. |
| `kind` | `NativeKind::as_str()` | `"intrinsic"`, `"bridge"`, `"synthetic-stub"`. |
| `registered_by` | `NativeCensusEntry::registered_by` | `#[track_caller]` `Location`, not a formatted string built per registration. `null` when not recorded. |
| `overwrote` | `NativeCensusEntry::overwrote` | Kind of the entry this registration replaced. This is how you find a `Bridge` that was silently downgraded by a later default-category `register()`. |
| `invocations` | `NativeMethodRegistry::record_invocation` | Per-slot relaxed counter, incremented by **every** dispatch path. A `SyntheticStub` with `invocations: 0` is dead-registration cleanup; one with a nonzero count is real behavioural debt. |
| `real_declaring_method` | boot-image lookup | The whole classification argument turns on this triple. `present:false` ⇒ the native is standing in for a class that does not exist. |
| `origin` | `ClassOrigin::as_str()` | `"boot-image"`, `"application-class-path"`, `"user-defined"`, `"vm-array"`, `"hidden-class"`, `"generated-lambda"`, `"generated-proxy"`, `"reflection-accessor"`, `"vm-internal"`, `"compatibility-stub"`. |

The `--jdk-only-report` file is a **separate, smaller** artifact
(`"schema_version": 1`, contract §9) carrying `violations` and `counts`. Do not
conflate the two.

### 3.5 Instrumentation completeness

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
  `Compatible` mode, per contract §5 — this is what makes the census meaningful
  before enforcement lands).

---

## 4. Coverage runs

```bash
# The frozen synthetic-stub census. Prints the live count before asserting.
cargo test -p cratonvm-native-builtins --test stub_ratchet -- --nocapture

# Registry API contracts.
cargo test -p cratonvm-native-api --test registry_contracts -- --nocapture

# Real-path per-subsystem smoke gates.
cargo test -p cratonvm-vm --test synthetic_diff -- --nocapture

# Deterministic end-to-end Java vectors, CratonVM vs HotSpot.
# NOTE: regression-suite is a shell harness, not a cargo crate.
CV="$PWD/target/release/cratonvm" JDK="$JAVA_HOME" bash regression-suite/run.sh

# HotSpot differential gate against the committed ledger.
cargo run -p cratonvm-difftest --bin cratonvm-difftest -- \
  gate --jdk "$JAVA_HOME" --corpus difftest/seeds
```

`difftest` mode labels today are `jit-on`, `nojit`, `no-intrinsics`,
`moving-gc`, `low-jit-threshold` (`difftest/src/runner.rs`, `Mode::label`), with
`--modes` defaulting to `jit-on,nojit`. Strict-mode labels are a **projection**,
not a repository claim — no `jdk-only-*` mode exists yet.

### 4.1 Reading the ratchet

`native-builtins/tests/stub_ratchet.rs` builds a fresh `NativeMethodRegistry`,
calls `register_essential_natives`, and counts `SyntheticStub` rows from
`dump_registrations()`. It asserts:

- `synthetic <= BASELINE_SYNTHETIC_STUBS` (currently **157**, plus a declared
  `SLACK`); and
- `total >= MIN_TOTAL_REGISTRATIONS` (**8,000**).

> **Discrepancy vs. the research report.** The report describes the baseline as
> "157 `SyntheticStub` entries among roughly 9,320 registrations." The 157 is
> exact and asserted. The 9,320 is **not asserted** — the test's own comment
> records that the denominator came from a historical argument and was never
> checked, which is why the floor is a loose 8,000. Do not quote 9,320 as a
> current measurement; re-derive it from `.counts.total` in a fresh
> `--dump-native-registry`.

The ratchet counts the **default build**. The `synthetic-jdk` override block is
feature-gated and deliberately not counted.

---

## 5. Classification rubric

Every census row resolves to exactly one disposition. Answer in order; stop at
the first rule that fires.

| # | Condition | Disposition |
|---|---|---|
| 1 | Real declaring class/method absent from the boot image, and the registration supplies compatibility behaviour | **CompatibilityShim** — forbidden under `JdkOnly` |
| 2 | Real method is `ACC_NATIVE` **and** the callback is a complete VM/OS implementation | **Bridge** |
| 3 | Real method has concrete bytecode **and** the callback is proven observationally equivalent (differential-tested) | **Intrinsic** |
| 4 | Real method has concrete bytecode **and** the callback is an incomplete replacement | **Delete from the strict path** — real bytecode wins |
| 5 | `invocations == 0` across the whole corpus, and no test invokes it | **Delete** — dead registration |
| 6 | The entry exists to stand in for a runtime-generated artifact (lambda, proxy, accessor) | **Move to the generated-class service** — it is not a native at all |

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

The class-file `ACC_SYNTHETIC` flag and the `Synthetic` attribute are **never**
a defect. `target/jdk-only-audit/classfile-synthetic-sites.txt` exists to make
that argument quickly, not to enumerate problems.

### 5.1 Worksheet generation

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

`registered_by` and `invocations` are `null` until the schema-v2 fields land;
the `//` fallbacks above keep the worksheet generatable against today's dump.

Feed each row through [`jdk-only-native-review.md`](jdk-only-native-review.md).

---

## 6. Per-JDK blocker artifacts

Publish two machine-generated files for every supported JDK image. No static
review can produce an exhaustive missing-method list across "any JDK" — native
surfaces and class-library internals vary by feature version, so the list is
generated per image and is only valid for that image.

```text
target/jdk-only-audit/jdk-<feature>-missing-natives.json
target/jdk-only-audit/jdk-<feature>-synthetic-dependencies.json
```

Practical initial matrix: **JDK 21 and 25**, Linux and Windows, with JDK 17 as a
compatibility anchor run nightly.

> **Discrepancy vs. the research report.** The report states that the
> repository's real-path CI uses JDK 21 and its differential CI uses JDK 25.
> As of this writing **every** `actions/setup-java` step in
> `.github/workflows/ci.yml` pins `java-version: '25'`, including
> `real-path-coverage` and `difftest-gate`. JDK 21 is a proposal here, not
> current CI coverage.

> **Second discrepancy.** The report states the native-stub census is advisory
> "because that job does not provision a JDK." Not so: the `Synthetic-stub
> ratchet` step lives in `build-and-test`, which does provision JDK 25, and the
> step carries no `continue-on-error` — it is blocking on `ubuntu-latest`. The
> only `continue-on-error: true` in `ci.yml` is on the `synthetic-jdk` feature
> job's test step. `difftest-gate` is likewise blocking.

---

## 7. Artifact inventory

A complete audit run leaves:

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
target/jdk-only-audit/registry-real.json
target/jdk-only-audit/registry-no-stubs.json
target/jdk-only-audit/registry-strict.json                 # wave 1
target/jdk-only-audit/classes-strict.json                  # wave 1
target/jdk-only-audit/report-strict.json                   # wave 1
target/jdk-only-audit/missing-real.json
target/jdk-only-audit/missing-real-by-module.json
target/jdk-only-audit/missing-no-stubs.json
target/jdk-only-audit/missing-strict.json                  # wave 1
target/jdk-only-audit/stub-review.tsv
target/jdk-only-audit/jdk-<feature>-missing-natives.json
target/jdk-only-audit/jdk-<feature>-synthetic-dependencies.json
```

`target/` is not committed. Upload these as CI artifacts on every strict-job
failure, and attach the relevant ones to any issue filed via
[`.github/ISSUE_TEMPLATE/jdk-only.yml`](../.github/ISSUE_TEMPLATE/jdk-only.yml).
