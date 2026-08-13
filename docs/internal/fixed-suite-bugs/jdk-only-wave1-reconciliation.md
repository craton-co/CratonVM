# JDK-only wave 1 — cross-agent reconciliation

Author: AA-RECONCILE. Read-only audit; no file outside this one was modified.

**Snapshot.** Working tree of `C:\craton\wt-jdk-only`, branch `feat/jdk-only-mode`,
`HEAD = ef75dd778`, 2026-07-31T16:58Z. Findings are against the **working tree**
(committed + uncommitted), because most of the feature is uncommitted.

**The tree moved while this audit ran.** `HEAD` advanced from `947ee1f5c` to
`ef75dd778`, `../../../vm-cli/src/main.rs` shrank by ~1,000 lines, and two new untracked
files appeared. Three defects I had already written up were fixed underneath me
and are recorded in §5 as *resolved during audit*, not as open findings. Anything
in §1–§3 was re-verified against the final snapshot immediately before writing.

Reference standard for every comparison: `../../feature-designs/jdk-only-mode.md`.

Nothing was built. `cargo` was not run (per instruction). Every claim below is
from reading source. Where I cannot tell whether something compiles without
building, I say so.

---

## 0. Summary

| Tier | Count |
|---|---|
| Hard compile errors (would fail `cargo build` / `cargo check --tests` / `clippy`) | **0** |
| Test-time failures (compile, then fail an assertion) | **4** |
| Behavioural / consumer divergences that still compile | **9** |
| Stale-comment / process divergences | **4** |

**Most likely first failure:** not a compiler error — it is
`cargo test --workspace` failing in `../../../vm/tests/jdk_only_config.rs`, four
assertions in three tests (§2.1–§2.4). All four are that test file asserting a
*different* `JdkOnlyViolation::render` / `to_json` surface than
`../../../types/src/error.rs` actually implements, and `../../../types/src/error.rs` has its own
unit tests pinning the shape it does implement — so the two test suites
contradict each other and one of them must lose.

If a compile error does appear first, the two places I could not fully rule out
are named in §4 (`../../../jfr/src/jdk_only.rs`, and the doctest in
`../../../cratonvm-embed/src/lib.rs`).

---

## 1. Hard compile errors

**None found.** Every contract-specified item was traced from definition to
every call site, and the arities, parameter types, return types, field names and
variant shapes agree. Detail of what was verified is in §4.

The one caveat: I verified type *names* and *shapes* by reading. I did not
type-check generics, lifetimes or trait bounds, and I did not run the borrow
checker over `resolve_dispatch`'s `&'a Method` borrow through the class-manager
read guard in `vm/src/runtime/interpreter/invoke.rs:11480`. That site holds `cm`
alive across the `resolve_dispatch` call and drops it before use, which is the
intended shape, but only a build proves it.

---

## 2. Test-time failures — compile, then fail

All four are the same disagreement: **`../../../vm/tests/jdk_only_config.rs` (agent A's
test file) was written against a `JdkOnlyViolation` diagnostic surface that
`../../../types/src/error.rs` (agent F) did not implement.** `../../../types/src/error.rs` carries
its own `#[cfg(test)]` tests (`types/src/error.rs:1593-1638`,
`remediation_ends_with_the_two_fixed_lines`) that pin the shape it *does*
implement, so this is not "one side is unfinished" — both sides are finished and
they disagree.

`cargo test --workspace` is a **blocking** CI step
(`.github/workflows/ci.yml:93-94`), so all four fail CI. Note the advisory
`jdk-only` job does **not** run this file (§3.9).

### 2.1 `render()` headline

- `vm/tests/jdk_only_config.rs:434` — `assert!(rendered.starts_with("CratonVM --jdk-only:"), …)`
- `types/src/error.rs:450` — `out.push_str(&format!("JDK-only policy violation [{}]\n", self.kind()));`

Contract §3 specifies only "multi-line operator-facing report, ending with the
`--real-jdk` fallback hint". It says nothing about a headline, so **the contract
supports `types`** and the test over-specifies.

Minimal fix: change the assertion at `jdk_only_config.rs:434` to
`starts_with("JDK-only policy violation [")`.

### 2.2 `to_json()` key/value spacing

- `vm/tests/jdk_only_config.rs:543` — `json.contains(&format!("\"kind\": \"{}\"", v.kind()))` (space after colon)
- `vm/tests/jdk_only_config.rs:565` — `v.to_json().contains("\"module\": null")` (space after colon)
- `types/src/error.rs:681-692` — `json_field` emits `json_string(key); out.push(':'); …` — **no space**, producing `"kind":"missing-native"` and `"module":null`.

Contract §3 says "hand-rolled to match existing dump style", no spacing
requirement. The compact form is also what the `--jdk-only-report` writer
consumes (`vm/src/vm/vm_init.rs:4158` re-indents by newline only, and the body
has no newlines), and `../../../difftest/src/census.rs` parses with `serde_json`, which
does not care. **Contract-neutral; `types` is the incumbent.**

Minimal fix: drop the two spaces in the test's expected substrings.

### 2.3 `render()` never shows `initiating_loader`

- `vm/tests/jdk_only_config.rs:470` — asserts the rendered report contains `"jdk/internal/loader/ClassLoaders$AppClassLoader"`, the `initiating_loader` of the sample `CompatibilityClassRequested` (`jdk_only_config.rs:286`).
- `types/src/error.rs:309-318` — `requested_from()` returns `requester` for that variant; `initiating_loader` is read by **nothing**. `render` (`types/src/error.rs:448-495`) prints class / member / requested-from / reason / JDK block / remediation. `to_json` does emit it (`types/src/error.rs:519-524`).

Contract §1.7 requires failures to name "class, method, descriptor, class
origin, attempted native kind, JDK feature version and the `--real-jdk`
fallback" — the initiating loader is not on that list, so **the contract
supports `types`**. But the field is then human-invisible: it exists in the JSON
and nowhere an operator reads. This is worth fixing on the `types` side rather
than the test side.

Minimal fix (preferred): add an `initiating loader:` line to `render` for
`CompatibilityClassRequested`, via a new `initiating_loader()` accessor next to
`requested_from()`. That satisfies the test and closes the invisible-field gap.

### 2.4 Two incompatible redaction policies

- `vm/tests/jdk_only_config.rs:500` — `assert!(quiet.contains("jdk-25"))` — "the last component is what identifies the JDK, so redaction must keep it".
- `vm/tests/jdk_only_config.rs:505` — `assert!(quiet.contains("--explain-jdk-only"))` — a redacted report must say how to get full paths.
- `types/src/error.rs:661-676` — `redact_paths` replaces the **whole whitespace-delimited token** with `<redacted>`; the basename is destroyed.
- `types/src/error.rs:175-182` — the two fixed remediation lines name `--real-jdk` and `--jdk-only-report <FILE>`; **neither mentions `--explain-jdk-only`**, and no other line does.
- `vm/src/vm/vm_init.rs:4609` — `redact_absolute_paths` produces `<redacted>/<basename>` — basename **kept**.
- `vm/src/vm/vm_init.rs:4509` — `redact_registration_site` produces `<redacted>/<file:line>` — basename **kept**.

So there are three redaction implementations and the `types` one is the odd
one out. Contract §3 only says "absolute paths must be redacted unless
`verbose`", which all three satisfy, so the contract does not decide it — but
the *repository* has converged on basename-preserving redaction in two places
and the test was written to that convention.

Worth noting: `render(_, false)` is **unreachable in production**.
`vm-cli/src/main.rs:2134-2141` calls `render(jdk_feature, true)` when
`--explain-jdk-only` is set and `redact_absolute_paths(&summary())` otherwise, so
`types`' own redaction path only ever runs from tests today.

Minimal fix: make `types::redact_paths` delegate to the same
`<redacted>/<basename>` rule (a ~6-line change, no new dependency), and append
`--explain-jdk-only` to the remediation block when `!verbose`. Then §2.4 passes
and the three implementations agree.

---

## 3. Behavioural divergences (these compile)

### 3.1 `ClassOriginEntry::requested_by` is structurally always `null`

- Producer: `classloading/src/class_manager.rs:2438` — `requested_by: None`, hard-coded, with a `JDK-ONLY-NOTE` (line 2428) handing the job to the §7 interpreter agent.
- Second ask: `classloading/src/class_manager.rs:2488` — `requester: None` on every `CompatibilityClassRequested` the class manager records, same reason.
- Consumers: `vm/src/vm/vm_init.rs:4802` writes `"requested_by": …` (always `null`); `../../../tools/jdk-only-blockers/blockers.py` reads the field; the contract's §5 row shape promises it.

**Not satisfied.** No interpreter-side code populates either field. Every
`compatibility-class-requested` violation in a real report will therefore say
`"requester": null`, and `blockers.py`'s attribution column is empty for the
whole class-origin half. The `--jdk-only-report` violation rows that *do* carry a
requester can only come from a caller that constructs the violation itself, and
there is none.

Impact: the class-origin census answers "what was fabricated" but not "who asked",
which is half of §1.7's actionability requirement.

### 3.2 `JdkOnlyViolation::MissingBootClass` has no producer

Grep over the whole tree: the variant is constructed only in
`vm/tests/jdk_only_config.rs:314`, `vm/tests/jdk_only_config.rs:487` and the
un-wired `../../../jfr/src/jdk_only.rs`. Production code never builds one.

Contract §8 permits either `MissingBootClass` **or** `InvalidConfiguration` for
the strict boot precondition, and `vm/src/vm/vm_init.rs:429` chose
`VmError::InvalidConfiguration`. So this is contract-compliant, but the variant
is dead: a consumer written to tally `missing-boot-class` will always see zero.
Worth stating in the docs rather than fixing.

### 3.3 `NativeShadowsBytecode` is produced only by the JIT

Sole construction site: `jit/src/lib.rs:4134`. The interpreter's §7 routing
(`resolve_dispatch`, `resolve_native_dispatch_wave1`) never emits it — it
returns `None`/`Bytecode` instead when bytecode wins, which is the wave-1
"measurement, not deletion" posture but means the counter under-reports.

`vm/src/vm/vm_exec.rs:319` (`NativeKind::Bridge if bytecode_available => None`)
is the exact place where a `NativeShadowsBytecode` violation *would* be recorded
if wave 1 wanted the measurement. It silently declines instead. Contract §10
allows this ("record the violation, keep going, and count it" is permitted, not
mandated, outside §4/§5), so this is a gap, not a violation.

### 3.4 Two independent notices for `CRATONVM_REAL=-stubs`

- `vm-cli/src/main.rs:2053` `note_no_stubs_env_without_jdk_only`, called at `vm-cli/src/main.rs:2811`, prints a 4-line note.
- `types/src/flag_groups.rs:838-847` `SUPERSEDED` + `process_env_supersessions`, consumed at `../../../vm-cli/src/main.rs` (the `for superseded in …process_env_supersessions()` loop), prints a one-line note.

Both fire for the same environment (`CRATONVM_REAL=-stubs` without
`--jdk-only`), so an operator sees the advice twice, in two different wordings.
Contract §9 asks for "a one-time note".

Minimal fix: delete `note_no_stubs_env_without_jdk_only` and keep the
`flag_groups::SUPERSEDED` mechanism (it is the general one and it already has a
polarity-sensitive test at `types/src/flag_groups.rs:1481`), or vice versa —
but not both.

### 3.5 `SharedVm::dump_native_registry_json` is now dead and its doc is wrong

- `vm/src/vm/vm_init.rs:3757` — the function exists and delegates to `dump_native_census_json(path, false)`.
- Its doc (`vm/src/vm/vm_init.rs:3747-3756`) claims "in-crate tests and embedders call this exact signature".
- Grep: **zero callers** anywhere in the tree, including tests. `--dump-native-registry` is served by `SharedVm::dump_native_census_json` through `vm-cli/src/main.rs:write_jdk_only_dumps`.

Not a defect; a stale claim on a now-unused shim. It will trip
`#[warn(dead_code)]`? No — it is `pub`, so it will not. It will simply mislead.

### 3.6 `ensure_synthetic_class` cannot enforce, only record

`classloading/src/class_manager.rs:2918` — `pub fn ensure_synthetic_class(&mut self, name: &str, num_fields: usize) -> ClassId`. It returns a bare `ClassId`, so the
`Err` from `admit_compatibility_class` (`class_manager.rs:2474`) cannot be
propagated; under `JdkOnly` the violation is recorded and the class is
fabricated anyway.

Already written up by its author in
`docs/known-issues/jdk-only/ensure-synthetic-class-cannot-enforce-only-record.md`.
Consequence for the CI gate: the "zero compatibility classes" assertion in
`.github/workflows/ci.yml:487-505` can fail even when the policy is working,
because this path still mints stubs. Two of the three test files
(`../../../classloading/tests/jdk_only_class_origin.rs`) route around it by calling
`load_class`, which *does* return `Result`.

### 3.7 `resolve_dispatch` deviates from the normative §7 order — deliberately

`vm/src/vm/vm_exec.rs:215-227` inserts a step between §7's step 3 and step 4: a
method with **no `Code`** but a registered native dispatches to that native
instead of `Reject(MissingImplementation)`. The deviation is documented in the
function banner (`vm_exec.rs:159-177`) and justified (interface-level bridges
would otherwise fail to boot in *both* modes).

This is a real divergence from the contract text and the orchestrator should
either bless it in the contract or schedule its removal. It does not weaken
§1.3 (`SyntheticStub` is still refused on that branch) or §1.4 (nothing is being
shadowed, since `code()` is `None`).

### 3.8 `resolve_native_dispatch_wave1` is an API the contract does not mention

`vm/src/vm/vm_exec.rs:279` adds a 7-argument name-only adapter alongside the
contract's 4-argument `resolve_dispatch`. Eight call sites use it
(`vm/src/jit/helpers.rs:6047`; `vm/src/runtime/interpreter.rs:5627`;
`vm/src/runtime/interpreter/invoke.rs:10694`, `:11638`, `:12650`;
`vm/src/vm/vm_exec.rs:13637`, `:21388`, `:21509`) and exactly one site uses
`resolve_dispatch` proper (`vm/src/runtime/interpreter/invoke.rs:11480`).

All eight call sites pass 7 arguments and type-match the signature — this is
**not** a mismatch. It is a contract-surface addition the orchestrator should
know about, because §7 says "THE single native-vs-bytecode decision point" and
there are now two entry points with different admission logic
(`compat_native_wins` is an input to one and not the other).

The `Compatible`-mode behaviour of the adapter is a pure function of
`compat_native_wins` (`vm_exec.rs:296-307`), which is what keeps wave 1
bit-for-bit — that reasoning is sound as written.

### 3.9 The advisory `jdk-only` CI job does not run `jdk_only_config`

`.github/workflows/ci.yml:441-445` runs `jdk_only_registry`,
`jdk_only_class_origin`, `jdk_only_dispatch` and `stub_ratchet`.
`.github/workflows/ci.yml:437-438` says to add `--test jdk_only_config` "when
`../../../vm/tests/jdk_only_config.rs` … lands". It landed (commit `fcd11e0cc`).

Consequence: the four §2 failures surface only in the blocking
`cargo test --workspace`, not in the job named after the feature.

---

## 4. Category-by-category verdict

### 4.1 Type and signature agreement across crate boundaries — **CHECKED, clean**

Every contract-specified item traced from definition to all call sites:

| Item | Definition | Verdict |
|---|---|---|
| `CompatibilityMode` (+`as_str`, `is_jdk_only`, `Default`) | `types/src/compat.rs:28,43,51` | matches §3 exactly |
| `ExecutionPolicy` (+`compatible`, `jdk_only`, `is_jdk_only`, `Default`) | `types/src/compat.rs:58,67,76,83,88` | matches §3 |
| `JdkOnlyViolation`, 7 variants + field names | `types/src/error.rs:193-247` | matches §3 field-for-field |
| `kind` / `summary` / `render` / `to_json` / `Display` | `types/src/error.rs:255,389,448,504,604` | signatures match; behaviour differs from one test — §2 |
| `VmError::{InvalidConfiguration, JdkOnly}` | `types/src/error.rs:125,135` | present; `From<JdkOnlyViolation>` hand-written at `:145` (correct — `#[from]` would demand `Error`) |
| `NativeKind::allowed_in` | `native-api/src/registry.rs:4157` | matches §4 |
| `NativeCensusEntry` (7 fields) | `native-api/src/registry.rs:4180-4195` | matches §4 |
| `NativeMethodRegistry::{set_compatibility_mode, compatibility_mode, refused_registrations, census, invocations_of_kind, record_invocation}` | `registry.rs:4525,4532,4538,4613,5552,5517` | all six present, signatures match §4; `record_invocation` takes `&self` as required |
| `find_with_kind` unchanged | `registry.rs:5383` | unchanged, as §4 demands |
| `ClassOrigin`, 10 variants | `classloading/src/class_origin.rs:36-84` | matches §5 variant-for-variant |
| `ClassOrigin::{as_str, allowed_in, is_compatibility_stub}` + `Default = VmInternal` | `class_origin.rs:91,112,123,165` | matches §5 |
| `ClassOriginEntry` (6 fields) | `class_origin.rs:175-189` | matches §5 |
| `Class::origin` + `Class::set_origin` | `classloading/src/class.rs:343,508` | matches §5; `is_synthetic_stub` retained as required |
| `ClassManager::{set_compatibility_mode, compatibility_mode, dump_class_origins, origin_violations}` | `class_manager.rs:2405,2410,2421,2450` | all four present, signatures match §5 |
| `VmConfig::{is_jdk_only, execution_policy, validate_compatibility}` + `compatibility_mode` field | `../../../vm/src/config.rs` (`with_compatibility_mode`, `is_jdk_only`, `execution_policy`, `validate_compatibility`) | matches §6 |
| `DispatchDecision<'a>`, 4 variants | `vm/src/vm/vm_exec.rs:62-74` | matches §7 |
| `resolve_dispatch` | `vm/src/vm/vm_exec.rs:178-183` | 4 params, types match §7 (`Method` aliased to `ClassFileMethod` at `vm_exec.rs:54`) |
| `resolve_native_dispatch_wave1` | `vm/src/vm/vm_exec.rs:279-287` | not in contract — see §3.8; all 8 call sites pass 7 matching args |
| `dispatch_policy` | `vm/src/vm/vm_exec.rs:327` | `&SharedVm -> ExecutionPolicy` |
| `record_native_dispatch` | `vm/src/vm/vm_exec.rs:342` | 4 params; one call site (`invoke.rs:13013`) |

Re-exports verified: `types/src/lib.rs:34` (`pub mod compat;` + root re-export),
`../../../native-api/src/lib.rs` (`NativeCensusEntry` added to the `pub use` list),
`../../../classloading/src/lib.rs` (`pub mod class_origin;` + `pub use
class_origin::{ClassOrigin, ClassOriginEntry}`), `../../../vm/src/config.rs`
(`pub use cratonvm_types::compat::{CompatibilityMode, ExecutionPolicy}`),
`vm/src/vm.rs:22-23` (`pub use vm_exec::*; pub use vm_init::*;`),
`cratonvm-embed/src/lib.rs:156`.

`jit-api/src/lib.rs:416-478` adds `CachedBytecodeMethod::native_dispatch`, which
duplicates the pre-existing `NativeCallSite::callback_with_kind`
(`native-api/src/native_id.rs:334`) plus a `record_invocation`. Not a conflict,
but two near-identical helpers now exist.

### 4.2 `Class { … }` struct-literal fallout — **CHECKED, clean**

All 50 `Class`-literal sites (identified by the mandatory `is_synthetic_stub:`
initialiser, which no literal can omit) set `origin:`. Per-file counts of
`is_synthetic_stub:` initialisers vs `origin:` initialisers match in
`access_control.rs`, `bytecode_verifier.rs`, `class.rs`, `class_manager.rs`,
`verifier.rs`, `wp2_10_nest_host.rs`, `vm_benchmarks.rs`, `stackwalker.rs`,
`vm_exec.rs`, `vm_util.rs`, `vm.rs`.

No functional-update (`..base`) `Class` literal exists, so the initialiser count
is an exact proxy.

The exhaustive destructure/rebuild in
`classloading/src/class_manager.rs:2035` (`RedefineInvariantSnapshot::from_class`,
deliberately written without a trailing `..` so a new field is a hard error)
handles `origin` at `:2107`.

The `Class` literal in `../../../vm/src/vm/vm_exec.rs`'s `#[cfg(test)] mod tests` was
missing `origin` earlier in this session and now sets it
(`vm_exec.rs:23888`, immediately above `is_synthetic_stub` at `:23889`). See §5.

### 4.3 JSON shape agreement — **CHECKED; the known divergence was resolved during the audit**

The task brief states two divergent `schema_version: 2` native-census writers
exist. **They did, and they no longer do.** They were collapsed to one while this
audit was running. Both the finding and the resolution are recorded here because
the orchestrator was told to expect the divergence.

**What it was** (`../../../vm-cli/src/main.rs`'s `write_native_census_json` vs
`SharedVm::dump_native_census_json`, both stamping `"schema_version": 2`):

| Aspect | launcher writer (deleted) | `vm` writer (survived) |
|---|---|---|
| top-level `"mode"` | absent | present (`vm_init.rs:3876`) |
| row sort key | `(class, name, descriptor, registered_by)` | `(class, name, descriptor)`, stable → registration order for ties |
| `real_declaring_method` | always `null` | `{loaded, declared, acc_native, has_code}` object |
| `registered_by` redaction | `<redacted>/<basename>` | same (`redact_registration_site`) |
| `counts` / `invocations` blocks | identical | identical |

Which consumers would have broken under which writer:
`../../../difftest/src/census.rs` — **neither**; it is shape-tolerant
(`find_row_array`, `census.rs:180`) and only reads `kind` + `invocations`.
`tools/jdk-only-blockers/blockers.py:497-517` — **neither**; it copies
`real_declaring_method` verbatim and tolerates `null`.
`.github/workflows/ci.yml:466-483` and `scripts/jdk-only-census.sh:210-213` —
**neither**; both `grep -o '"synthetic-stub": *[0-9]*' | head -n 1`, which hits
the `counts` block first under both writers.
The real breakage was for a human or a future strict parser: two files with the
same `schema_version` and different row sets is unresolvable.

**Current state (verified at `ef75dd778`):** `vm-cli/src/main.rs:2082-2099`
carries a "JDK-ONLY CENSUS WRITERS: there are none here any more" block; all
three artefacts are written by
`SharedVm::dump_class_origins_json(path, verbose)` (`vm_init.rs:4021`),
`SharedVm::dump_native_census_json(path, verbose)` (`vm_init.rs:3853`) and
`SharedVm::dump_jdk_only_report_json(path, verbose)` (`vm_init.rs:4088`), called
from `vm-cli/src/main.rs:write_jdk_only_dumps`. Signatures and call sites match.

Remaining JSON observations, all benign:

- **`scripts/jdk-only-census.sh:217`** used `grep -c '"compatibility-stub"'`,
  which reports N+1 because the tag is seeded into `counts` even at zero. Fixed
  during the audit (see the comment now at `scripts/jdk-only-census.sh:510`).
- **`.github/workflows/ci.yml:494-495`** still claims "a `counts` key is emitted
  only for origins that actually occurred, so an absent key is a real zero".
  That is **false** — `render_class_origins_json` (`vm_init.rs:4775`) and
  `dump_native_census_json` both seed the full tag vocabulary to zero, so the key
  is always present. The step's logic (`[ -n "$n" ] || n=0`) is still correct;
  only the rationale is wrong. Fix the comment.
- The `--jdk-only-report` violation list is now sorted by `(kind, summary)`
  (`vm_init.rs:4118`), matching `../../CONFIG.md`'s "Violations are sorted so the
  file is diff-stable". Agreed.
- `JdkOnlyViolation::to_json` returns a **complete object including braces**
  (`types/src/error.rs:508,599`), and both the report writer
  (`vm_init.rs:4151-4158`) and `difftest/src/census.rs:207` treat it that way.
  Contract §3's phrasing ("JSON object body … no outer braces omitted") is
  ambiguous but the two sides agree.

### 4.4 Tag-vocabulary agreement — **CHECKED, clean**

Swept every literal spelling of the four vocabularies across `*.rs`, `*.py`,
`*.sh`, `*.yml` (docs excluded from the "disagreement" verdict but spot-checked).

- `CompatibilityMode::as_str` → `"compatible"` / `"jdk-only"`
  (`types/src/compat.rs:45-46`). Literals agree at
  `difftest/src/ledger.rs:231,234` (`PROFILE_COMPATIBLE` / `PROFILE_JDK_ONLY`),
  `difftest/src/runner.rs:300-302,884-889`, `difftest/src/census.rs:281,390-423`,
  `cratonvm-embed/src/lib.rs:326`, `vm/tests/jdk_only_config.rs:244-245`,
  `types/src/lib.rs:116`. **No disagreement.**
- `NativeKind::as_str` → `"intrinsic"` / `"bridge"` / `"synthetic-stub"`
  (`native-api/src/registry.rs:4139-4141`). Literals agree at
  `native-api/tests/jdk_only_registry.rs:256-258`,
  `tools/jdk-only-blockers/blockers.py:79-80,502,512`,
  `difftest/src/census.rs:42-44`, `vm/src/vm/vm_init.rs:3881-3903` (uses
  `kind.as_str()`, not literals), `.github/workflows/ci.yml:471`,
  `scripts/jdk-only-census.sh:211`. **No disagreement.**
- `ClassOrigin::as_str` → the ten tags (`classloading/src/class_origin.rs:93-102`).
  Duplicated as literal arrays in `vm/src/vm/vm_init.rs:4481`
  (`CLASS_ORIGIN_TAGS`, 10 entries, correct spellings, declaration order) and
  `tools/jdk-only-blockers/blockers.py:93-101`. `../../../.github/ISSUE_TEMPLATE/jdk-only.yml`
  agrees. **No disagreement.** The `vm_init.rs` array is a hand-maintained
  duplicate of the enum with a comment saying so (`vm_init.rs:4479-4480`) —
  correct today, unenforced.
- `JdkOnlyViolation::kind` → the seven tags (`types/src/error.rs:257-263`).
  Literals agree at `vm/tests/jdk_only_config.rs:344-351`,
  `.github/ISSUE_TEMPLATE/jdk-only.yml:68-74`, `difftest/src/census.rs:347-349`,
  `difftest/src/harness.rs:1433-1499`, `difftest/src/ledger.rs:727-736`,
  `tools/jdk-only-blockers/blockers.py:105-106` (which uses its *own* category
  names `synthetic-stub-registration` / `compatibility-stub-class`, deliberately
  distinct — they are the tool's own taxonomy, not the enum's, and the tool never
  claims otherwise). **No disagreement.**

The report `counts` keys are snake_case (`synthetic_stub_invocations`, …) while
the census `counts` keys are the kebab-case `as_str` tags. That split is what
contract §9 spells and both writers and `difftest/src/census.rs:41-53` implement
it consistently.

### 4.5 Test-vs-implementation agreement — **CHECKED**

- `../../../vm/tests/jdk_only_config.rs` — **4 failing assertions**, §2.
- `../../../native-api/tests/jdk_only_registry.rs` — every API it calls exists with the
  right signature (`census`, `compatibility_mode`, `find`, `find_with_kind`,
  `invocations_of_kind`, `kind_of`, `record_invocation`, `refused_registrations`,
  `resolve_id`, `set_compatibility_mode`, `with_category`). The
  `#[track_caller]`-through-a-closure assumption at `jdk_only_registry.rs:60`
  (`with_category(kind, |r| r.register(...))`) is sound: the location is the
  textual `register(...)` call, which is in the test file, so
  `census_records_the_registration_site`'s `site.contains("jdk_only_registry.rs")`
  holds. `invocation_counters_survive_re_registration_on_the_winning_slot` relies
  on `register()` rewriting `slot.reg_index` in place — verified at
  `native-api/src/registry.rs:5265`. **No problems found.**
- `../../../classloading/tests/jdk_only_class_origin.rs` — `ClassManager::new(&[],&[],&[])`
  matches `class_manager.rs:2242`; `get_class_mut` exists at `:5512`;
  `ClassLoaderId::to_native_id` at `types/src/class_id.rs:58`. The array-origin
  fixtures depend on `load_class("[Ljava/util/HashMap;")` reaching the
  `ClassOrigin::VmArray` literal at `class_manager.rs:7685` — it does, and that
  path does **not** go through `admit_compatibility_class`, so it succeeds under
  `JdkOnly` as the test requires. The strict-refusal fixtures depend on
  `create_synthetic_stub` (`class_manager.rs:7289`) calling
  `admit_compatibility_class` and propagating `?` — it does, at `:7320`.
  **No problems found.**
- `../../../vm/tests/jdk_only_dispatch.rs` — fixtures build `ClassFileMethod` and
  `CodeAttribute` literals; field sets match `reader/src/method.rs:18-23` and
  `reader/src/attribute.rs:288-294`; `LazyAttribute::new_decoded`
  (`attribute.rs:616`) and `ByteView::from_vec` (`byte_view.rs:212`) exist, and
  `ClassFileMethod::code()` (`method.rs:48`) reads through `as_decoded`, so the
  `Decoded` fixture is visible to §7 step 3. `owner_class` uses
  `ensure_synthetic_class` under the default `Compatible` mode. **No problems
  found.**
- `../../../native-builtins/tests/stub_ratchet.rs` — `cratonvm-types` is a regular
  dependency of `native-builtins` (`../../../native-builtins/Cargo.toml`), so
  `use cratonvm_types::compat::CompatibilityMode` resolves in an integration
  test. The three new tests' one-sided bounds
  (`dropped >= compat_stubs`, `refused <= compat_stubs`) are consistent with
  `register()`'s refusal path and `alias_class`'s log-walking behaviour.
  `BASELINE_SYNTHETIC_STUBS = 157` and `MIN_TOTAL_REGISTRATIONS = 8_000` are
  unchanged; the new `STRICT_MIN_TOTAL_REGISTRATIONS = 7_500` is a floor, not a
  measurement. **No problems found.** (Deliberately not asserting any total
  registration figure here — the ratchet asserts 157 exactly and `total >= 8_000`,
  nothing more.)
- **No fixture assumes a code path that does not exist.**

### 4.6 Unresolved cross-agent asks (`// JDK-ONLY-NOTE:`) — **CHECKED**

Six markers in `*.rs`. Only two are cross-file asks; both are open.

| Location | Ask | Satisfied? |
|---|---|---|
| `classloading/src/class_manager.rs:2428` | interpreter (§7) must populate `ClassOriginEntry::requested_by` | **No** — §3.1 |
| `classloading/src/class_manager.rs:2488` | interpreter (§7) must populate `CompatibilityClassRequested::requester` | **No** — §3.1 |
| `vm/src/native/jni.rs:1265,1268` | none — explicitly labelled pre-existing, orthogonal, not fixed here | n/a |
| `vm-cli/src/main.rs:2102` | wave-2 change to `classloading` + `native-api` for a live violation sink | No, and correctly scoped to wave 2 |
| `jfr/src/jdk_only.rs:1013` | inside an un-wired file (§4.7) | n/a |

The larger cross-owner list is the `JDK-ONLY-NOTE` block at
`jit/src/lib.rs:4178-4227`, six numbered obligations against files `jit` does not
own:

1. `jit_invoke_dispatch` / `jit_invoke_virtual_mic` route through the resolver —
   **satisfied** (`vm/src/jit/helpers.rs:6047` calls
   `resolve_native_dispatch_wave1`, and `admit_jit_fast_native`
   / `admit_jit_fast_native_resolved` at `helpers.rs:6095,6121` resolve the
   `NativeMethodId` for the census increment).
2. JIT-only native fast paths carry a `NativeKind` — **satisfied** for the paths
   funnelled through `admit_jit_fast_native`; I did not individually verify all
   four named callbacks (`hashmap_native_callback`, `matcher_native_callback`,
   `stringbuilder_native_callback`, the `ClassLoader.getResource*` intercept)
   reach it. Partial.
3. `build_helpers` must call `set_jit_execution_policy` before the first
   compilation — **satisfied**, `vm/src/jit/helpers.rs:11663`, with an explicit
   ordering comment at `:11637`.
4. `cp_elidable_init_resolver` must check for a native shadow before eliding a
   constructor — **NOT satisfied**. `resolve_jit_elidable_init_loading`
   (`vm/src/runtime/interpreter/invoke.rs:15806`) contains no registry lookup
   and no native-shadow test anywhere in its body. This is the known
   `jit-elidable-ctor-must-check-native-shadow` defect, still open.
5. `INDY_STRING_CONCAT_FN` — deliberately no action. Matched by
   `vm/src/jit/helpers.rs:11695`.
6. Monitor direct fns — no action needed. Matched by the same comment.

A **seventh marker vocabulary** appeared that the brief does not mention:
`JDK-ONLY-CLASSIFY` (e.g. `native-io/src/pipe.rs:767`, `../../../native-collections/src/lib.rs`,
`../../../native-awt/src/natives.rs`) — per-registrar classification requests for the
wave-2 stub reclassification. They are self-contained comments, no cross-file
ask, no compile impact.

`JDK-ONLY-WAVE2` markers are present and mechanically greppable as §7 requires
(33 sites across `classloading`, `jit`, `jit-api`, `vm`).

### 4.7 Not fully checkable / partially checked

- **`../../../jfr/src/jdk_only.rs` — untracked, ~1,400 lines, NOT wired.** `../../../jfr/src/lib.rs`
  has no `mod jdk_only;` (`lib.rs:81-86` lists `builtin, dump, event, recording,
  repository, stream`), so the file is not compiled and cannot cause a build
  error today. If it is wired as-is it **will** fail: it does
  `use cratonvm_types::error::JdkOnlyViolation` (`jdk_only.rs:116`) and
  `../../../jfr/Cargo.toml`'s `[dependencies]` contains no `cratonvm-types`. It appeared
  mid-session; treat as work-in-progress, not a defect. Two things must land
  together: the `pub mod jdk_only;` line and the `cratonvm-types` dependency.
- **`../../../scripts/jdk-only-bench.sh`** — untracked, appeared mid-session, not reviewed.
- **`cratonvm-embed/src/lib.rs:74-110` doctest** — I read it and every symbol it
  names exists (`VmConfig::with_host_jdk_default` at `vm/src/config.rs:835`,
  `Clone` on `VmConfig` at `config.rs:300`, the re-exports at
  `cratonvm-embed/src/lib.rs:156`). I cannot confirm a doctest compiles without
  running it.
- **`libcratonvm`** — the three new C entry points declared in
  `../../../libcratonvm/include/cratonvm.h` (`cratonvm_create_with_compatibility`,
  `cratonvm_compatibility_mode`, `cratonvm_compatibility_mode_supported`) all
  exist in `../../../libcratonvm/src/lib.rs` (`:1480`, `:2007`, `:2040`) and all five new
  symbols are in `../../../libcratonvm/cbindgen.toml`'s `include` list. Header/impl agree.
  I did not verify the generated-vs-committed header is byte-identical (that
  needs a cbindgen run).
- **`../../../regression-suite`** — I verified `CRATONVM_ARGS` forwarding exists
  (`regression-suite/run.sh:201`) and the `--jdk-only`-implies-strict-corpus
  selection (`run.sh:83`). I did **not** review the 21 new `RJdk*.java` vectors
  or the module fixture for correctness.
- **`difftest` runner/harness/ledger/oracle** — I verified the four new
  `Mode` variants, their labels, `cli_args`, `env_overrides`, `jdk_profile`,
  `collects_census`, `from_label` round-trip (`difftest/src/runner.rs:212-336`)
  and the `(class, jdk_profile)` ledger keying. I did **not** audit the ledger
  migration or the `../../../difftest/ledger.json` diff.
- **`../..`** — the ~15 new documents were not proof-read against the code
  beyond the specific claims cited above.
- **Borrow-check / lifetime correctness anywhere.** Not checkable without a build.

### 4.8 Not checked at all

- `../../../jit/src/lib.rs`'s 427 changed lines beyond the `JDK-ONLY-NOTE`/`WAVE2` markers,
  `set_jit_execution_policy` (`jit/src/lib.rs:4042`) and the
  `NativeShadowsBytecode` construction at `:4134`.
- `../../../vm/src/vm/vm_object.rs`, `../../../vm/src/vm/vm_util.rs` beyond the `Class` literals.
- `../../../.github/ISSUE_TEMPLATE/jdk-only.yml` beyond its tag vocabulary.
- `../../../tools/jdk-only-blockers/selftest.py` (490 lines).
- `docs/benchmarks/jdk-only.md`, `../../security/jdk-only-threat-model.md`.

---

## 5. Resolved during the audit (do not re-fix)

These were real at the start of this session and are fixed at `ef75dd778`. They
are listed so the orchestrator does not chase them from a stale note.

1. **`../../../vm/src/vm/vm_exec.rs`** — the `Class { … }` literal in the
   `#[cfg(test)] mod tests` block did not set `origin`, which would have failed
   `cargo test -p cratonvm-vm` and `cargo clippy --all-targets`. Now sets it at
   `vm_exec.rs:23888`.
2. **Two divergent `schema_version: 2` writers** — collapsed onto the `vm` side.
   See §4.3 for the characterisation the brief asked for.
3. **`vm-cli` calling `redact_absolute_paths` without importing it** — appeared
   for a few minutes during the writer consolidation; now fully qualified at
   `vm-cli/src/main.rs:2140`.

---

## 6. Stale comments that will mislead the next reader

1. `.github/workflows/ci.yml:404,510` — "`CRATONVM_ARGS` forwarding in
   `../../../regression-suite/run.sh`: NOT PRESENT" and "this step currently runs the
   ordinary compatible-mode suite". **It is present** (`regression-suite/run.sh:201`).
   The step is now genuine strict-mode evidence and the comment says not to trust it.
2. `.github/workflows/ci.yml:437-438` — "Add `--test jdk_only_config` here when
   `../../../vm/tests/jdk_only_config.rs` … lands". It landed. See §3.9.
3. `.github/workflows/ci.yml:494-495` — the "absent key is a real zero" rationale
   is false; counts are seeded. See §4.3.
4. `vm/src/vm/vm_init.rs:3747-3756` — `dump_native_registry_json`'s doc claims
   in-crate tests and embedders call it; nothing does. See §3.5.

Separately, `cargo fmt --all --check` is CI step 1
(`.github/workflows/ci.yml:55-56`) and this repository is not rustfmt-clean, so
every gate after it — including `cargo clippy` and `cargo test --workspace` —
is unreachable on a fresh CI run regardless of this feature. That is
pre-existing, not caused by wave 1, but it means "CI is green" cannot be used as
evidence that §2's four failures are absent.
