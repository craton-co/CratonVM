# Semantic Differential Fuzzer vs HotSpot

> Industrialize the manual "run it on HotSpot and eyeball the diff" loop that
> has produced nearly every bug logged in `MEMORY.md`. Generate
> and mutate Java programs (and bytecode), run them on **both** CratonVM and a
> real JDK, and **diff observable behavior** — stdout/stderr, thrown exception
> type+message, return value, and process exit code — automatically, with
> corpus growth, crash minimization, and an advisory-to-blocking CI gate path.
>
> **Goal · Current state · Design · Incremental delivery · Risks · Validation ·
> Scaffolding to land first.**

---

> **Status update (2026-07-02).** `difftest/` is now a workspace member package
> named `cratonvm-difftest`, and its installed binary is also named
> `cratonvm-difftest` to satisfy the repository's unique-binary rule. Its CI job
> is advisory (`continue-on-error`) while cross-platform ledger stability is
> proven. `fuzz/` remains a separate standalone workspace with 11 declared
> targets; treat fuzz-build status as advisory release evidence until the
> current fuzz-smoke job is green on the release commit.

## 1. Problem & motivation

CratonVM is validated against HotSpot almost entirely by hand. Every entry in
`MEMORY.md` follows the same loop: write a tiny `scratch/.../Repro.java`, run it
on `java` (JDK 25 at `C:\Program Files\Java\jdk-25`), run it on `cratonvm`, and
**eyeball** whether stdout / the exception / the checksum match. Examples that
this exact loop caught — and that an automated fuzzer would have caught earlier
and without a human:

- **GC corruption surfacing as a value diff** — bt18 checksum `68332206`
  (HotSpot) vs an under-count under moving young-gen (`project_precise_jit_stack_maps`,
  `default-moving-young-gen.md`).
- **JIT mis-optimization changing a return value / NPE** — kafka bug-25 escape
  analysis scalar-replaced an escaping arg → JIT path returned `null`
  (`reference_kafka_*`, fixed at dev `7d804d60`); bug-H/bug-I JIT catch-bypass
  and null-check-elim; bug-24 inline-cache UAF SIGSEGV.
- **indy / reflection / lambda corners** — JEP-358 helpful-NPE message text must
  be byte-identical to HotSpot (41-case probe, `jep358-helpful-npe.md`);
  `getGenericSuperclass`/bridge-method reflection cascades (`reference_bug06_*`);
  `forLanguageTag` CCE; record `toString` virtual-component dispatch.
- **Exception identity & ordering** — Throwable stacktrace order reversed
  (`hibernate-throwable-stacktrace-order-reversed-FIXED.md`); helpful-NPE owner
  rendering.

These are **semantic** divergences: the program parses and runs, but produces a
different *observable result*. They are exactly what the existing fuzz crate
does **not** test: the `fuzz/` targets are **panic-only parser fuzzers**
("arbitrary input may produce `Err(_)`, but must never panic") over untrusted
*byte* surfaces (class file, jimage, ASN.1, keystore, TLS record). Useful for
memory safety; blind to "the VM ran the program and got `43` where HotSpot got
`42`."

The thesis of this doc: the manual loop is already a differential oracle. We
have a working — but tiny and hand-curated — in-repo harness
(`vm/tests/differential.rs`) that does exactly stdout+return-value diffing
against `java`/`javac` on PATH. The work is to (a) make the *oracle* robust
(exit code, exception identity, stderr, timeout/hang, JIT-on/off, GC-mode),
(b) feed it a **generator** instead of a hand-written method list, (c) add
**minimization** and a **regression corpus**, and (d) wire it into the existing
HotSpot-baseline tooling and CI.

---

## 2. Current state in the codebase (what actually exists)

### 2.1 A working semantic differential harness — small and manual

`vm/tests/differential.rs` (642 lines) is the seed of this whole feature and is
**already real**:

- `differential_run(class, method, descriptor) -> DiffResult` — runs a Java
  static method under CratonVM and HotSpot and compares.
- `run_cratonvm(...)` runs **in-process** via `cratonvm_vm::vm::Vm::new(config)`
  + `vm.invoke(class, method, descriptor, &args)`, capturing
  `vm.main_thread.printed_lines` and the returned `Value` (`format_value`).
- `run_hotspot(...)` shells out to `java`/`javac` on PATH: for `main` it runs
  the class directly; for value-returning methods it **generates, compiles, and
  runs a `DiffWrapper__` wrapper** that `System.out.println`s the result.
- `Outcome { stdout, return_value }`, `DiffResult { ..., matches }`, and a
  serializable `Divergence` / `DivergenceReport` that **writes
  `bench/differential-divergences.json`** (`DivergenceReport::write_to_file`).
- Tests `diff_basic_arithmetic` and `diff_string_operations` over
  `cratonvm/DiffArithmetic` and `cratonvm/DiffString`, both
  `#[ignore = "requires java/javac on PATH"]`.

**Limitations (the gap this design closes):** the input is a *hand-written list*
of `(method, descriptor)`; the oracle compares only `stdout` + `return_value`
(no exit code, no exception **type/message**, no stderr); HotSpot is invoked
via a stringly-typed wrapper that only handles `()<prim>` and `main`; the
CratonVM side runs **in-process** so it cannot vary `CRATONVM_DISABLE_JIT` /
GC-mode per run (the env cache is a process-lifetime `OnceLock`, see §2.4);
there is no generator, no minimizer, no corpus, no CI hook.

### 2.2 The intrinsic differential pattern (subprocess, two-mode)

`vm/tests/intrinsic_diff.rs` (536 lines) already solves the "vary a
process-lifetime flag" problem that §2.1 cannot: it launches the **`cratonvm`
CLI binary as a subprocess** twice — once with `CRATONVM_DISABLE_INTRINSICS=1`
and once without — and asserts identical stdout. It documents binary resolution
(`CRATONVM_BIN` → `target/release/cratonvm[.exe]` → `target/debug/...`), a hard
`RUN_TIMEOUT = Duration::from_secs(120)` per child (to catch the megamorphic
inline-cache livelock), and the skip-if-not-built convention. This is the exact
process model the fuzzer's runner should adopt for *both* VMs.

`vm/tests/cluster_a_aqs_chm.rs` / `cluster_c_constructor.rs` (referenced there)
are further examples of the subprocess-launch pattern.

### 2.3 The JIT-internal differential (host IEEE-754 oracle)

`jit/tests/differential.rs` (360 lines) diffs the **JIT** against the host's
own f64/f32 evaluation of the same arithmetic (the interpreter follows the same
semantics), via `cratonvm_jit::x64::compile` + `is_jit_compatible`. Plus
`jit/tests/intrinsic_arraycopy.rs`, `intrinsic_crc32.rs`, `intrinsic_int_bits.rs`,
`intrinsic_long_bits.rs`, `intrinsic_string_search.rs`. These are a *self*-diff
(JIT vs interpreter/host), not vs HotSpot — but they establish the
"JIT-on vs reference must be identical" axis we want the fuzzer to drive at
scale, against HotSpot as the reference.

### 2.4 The CLI surface the fuzzer drives

`vm-cli` builds the `cratonvm` binary (`vm-cli/Cargo.toml`, `[[bin]] name =
"cratonvm"`; optional `java` alias under feature `java-bin-alias`). Its arg
normalizer (`vm-cli/src/main.rs`) is **`java`-compatible**: accepts `-cp` /
`-classpath` / `--classpath`, `-jar`, `-Xbootclasspath`, `-Xmx…`, `@argfile`,
and a main class — so **the same command line runs on `cratonvm` and on `java`**
(modulo the binary name). This is what makes a clean A/B runner possible: build
one arg vector, dispatch it to two executables.

Per-run behavior is switched by `CRATONVM_*` env vars cached once per process in
`vm/src/runtime/env_cache.rs` (`OnceLock`): `CRATONVM_DISABLE_JIT`,
`CRATONVM_JIT_THRESHOLD`, `CRATONVM_DISABLE_INTRINSICS`,
`CRATONVM_HELPFUL_NPE_OPCODES`, `CRATONVM_REAL_PROXY_SUPER`,
`CRATONVM_ROOTSNAP_CACHE`, plus the moving/selective-promote GC knobs referenced
in `MEMORY.md` (e.g. `CRATONVM_NO_SELECTIVE_PROMOTE`, `CRATONVM_SHADOW_STACK`).
Because the cache is process-lifetime, **each behavioral mode must be a fresh
subprocess** — exactly why §2.2's model wins over §2.1's in-process model.

### 2.5 The HotSpot-baseline tooling (perf, not behavior)

`scripts/capture-hotspot-baseline.{sh,ps1}` + `vm/src/bin/bench_hotspot_compare.rs`
are a **performance** pipeline: capture HotSpot C2 median-ns per kernel into
`bench/hotspot-baseline.json` (schema `Baseline { schema_version, host,
captured_at, metrics: { name: { median_ns } } }`), and `bench-hotspot-compare`
emits a `CratonVM/HotSpot` ratio table + geomean and gates at 1.5×
(`bench/baseline.json` vs `bench/hotspot-baseline.json`). This is **orthogonal**
to semantic diffing but is the template to reuse: the committed-JSON-baseline +
schema_version + host tag + CLI-gate-with-exit-codes shape is exactly what the
semantic fuzzer's *divergence ledger* and CI gate should imitate. The semantic
analogue of `hotspot-baseline.json` is `bench/differential-divergences.json`
(already produced by §2.1) — promote it to a first-class, committed
*known-divergence ledger*.

### 2.6 The parser fuzz crate (panic-only, to be reused for plumbing)

`fuzz/` (libFuzzer via `cargo +nightly fuzz`, 11 `[[bin]]` targets) fuzzes
*bytes* with a panic-only oracle. `fuzz-review.md` documents it has **no corpus,
no regression dir, no `fuzz.toml`, no CI hook, no oracle beyond panic**, and one
broken target. We **reuse its libFuzzer plumbing and corpus discipline** (seed /
artifact / `tmin` minimization / OSS-Fuzz `build.sh`) for the *bytecode* mutator
target (§4 step 5), but the semantic fuzzer's primary engine is generative +
process-level, not libFuzzer-in-proc (it must fork `java`, which libFuzzer's
in-process model forbids in the hot loop).

### 2.7 Test infra & app harness (for the macro tier)

`scripts/app-checker.sh` (smoke / functional / recursive / all over `apps/`),
`scripts/real-run-all.sh`, `test-infra/run-comparison-full.sh`,
`scripts/triage.sh` already run *real apps* under CratonVM and classify
rc/first-error. The fuzzer's "macro corpus" tier (whole programs from
`bench/*.java`, `apps/`) plugs into these rather than reinventing app launch.

---

## 3. Proposed design

Four cooperating pieces: a **generator**, a **two-VM runner**, a **diff oracle
+ minimizer**, and a **divergence ledger + CI gate**. All in one new crate
`difftest/` (a normal workspace member, unlike `fuzz/`), packaged as
`cratonvm-difftest` and driven by the `cratonvm-difftest` binary, reusing
`vm-cli`'s arg shape and §2.2's subprocess model. Earlier scaffold notes in
this design doc that mention a generic `difftest` binary are historical.

```
            ┌───────────── corpus ─────────────┐
            │ seeds/ (curated .java + .class)   │
 generator ─┤ regression/ (minimized repros)    │
 (grammar + │ mutations/ (live, coverage-fed)   │
  bytecode  └──────────────────────────────────┘
  mutator)            │  one program (source xor bytecode)
                      ▼
          ┌───── two-VM runner ─────┐   same argv, two executables
          │  run(cratonvm, env=M)   │   M ∈ {jit-on, --nojit, moving-gc, ...}
          │  run(java,     env=∅)   │   timeout, capture stdout/stderr/rc
          └────────────┬────────────┘
                       ▼  Observation × Observation
              ┌──── diff oracle ────┐  normalize → compare → classify
              │  stdout / stderr    │  (identity vs ordered vs canonicalized)
              │  exception type+msg │
              │  return / exit code │
              └─────────┬───────────┘
              divergence │             agreement
                         ▼
                 ┌── minimizer ──┐ shrink program; re-confirm divergence
                 └──────┬────────┘
                        ▼
          bench/differential-divergences.json  (ledger; known vs new)
                        │
                        ▼  difftest gate  →  exit code (CI)
```

### 3.1 Generator (corpus + generation strategy)

Three input tiers, smallest-blast-radius first:

1. **Curated seeds.** Re-use what exists: `vm/tests/resources/cratonvm/*.java`,
   `bench/*.java` (Scrabble, NBody, binarytrees, Matrix*, Test* …), and a new
   `difftest/seeds/`. Each seed is a self-contained program whose `main` prints
   a deterministic, fully-observable transcript (no wall-clock, no hashmap
   iteration order, no thread interleaving — see §3.3 determinism filter).

2. **Grammar-based source generation.** A small typed-expression generator
   (think a CratonVM-flavored Csmith/JustGen for the JVM) emits compilable Java
   whose `main` prints every intermediate. Generation is **weighted toward the
   bug history**: arithmetic edge cases (overflow, `Integer.MIN_VALUE / -1`,
   `% ` of negatives, `<<` shift-distance masking), the `indy` family
   (lambdas, `String` concat `invokedynamic`, record components, switch
   patterns), reflection (`getDeclaredMethod`, generic signatures, bridge
   methods), exceptions (nested try/catch/finally ordering, NPE on array vs
   field vs invoke for JEP-358 message parity), and GC pressure (allocate +
   retain + checksum, the bt-style oracle). Output is Java *source*; we then
   `javac` it once and run the **same `.class`** on both VMs (the wrapper-free
   path — far simpler than §2.1's `DiffWrapper__`).

3. **Bytecode mutation.** Start from a valid `.class` (seed or generated),
   apply *semantics-preserving-ish* and *semantics-perturbing* mutations
   (swap constant-pool ldc values, flip a branch opcode, reorder independent
   stack ops, inject a redundant `dup/pop`, change a method's invoke kind),
   re-verify with `classloading`'s bytecode verifier, and diff. This is the
   `cargo fuzz`-style coverage-fed tier and is where we reuse `fuzz/`'s
   libFuzzer plumbing (`fuzz_target!` over a structured `ClassFile` mutator),
   but with a **differential** oracle (run on both VMs) layered on top of the
   panic oracle. Only the *minimizer* and *replay* run `java`; the hot
   in-process libFuzzer loop stays panic-only for speed, and promotes
   interesting inputs to the slow differential tier.

The corpus on disk mirrors `fuzz/`'s discipline (which the review says is
*missing* there — we do it right here): `difftest/seeds/`,
`difftest/corpus/<tier>/`, `difftest/regression/` (every confirmed-then-fixed
divergence, minimized, committed so a future regression re-trips it).

### 3.2 Two-VM runner (the A/B executor)

A `Runner` that builds **one** java-compatible argv and dispatches it to two
executables, mirroring `intrinsic_diff.rs`'s subprocess model:

- **CratonVM side:** the `cratonvm` binary (resolved via `CRATONVM_BIN` →
  `target/release` → `target/debug`, as `intrinsic_diff.rs` already does),
  launched once **per behavioral mode** with the relevant `CRATONVM_*` env set
  (since §2.4's `OnceLock` cache means one mode per process). Modes:
  `jit-on` (default), `--nojit` / `CRATONVM_DISABLE_JIT=1`,
  `CRATONVM_DISABLE_INTRINSICS=1`, moving-GC vs selective-promote, low
  `CRATONVM_JIT_THRESHOLD` (force early compile to surface OSR/deopt bugs),
  `CRATONVM_REAL_PROXY_SUPER=1`. Each mode is its own row in the diff matrix —
  a JIT-only divergence (the dominant bug class) shows up as `jit-on ≠ java`
  while `--nojit = java`, which **auto-classifies the bug as JIT** (exactly the
  manual `--nojit` bisection done all over `MEMORY.md`).
- **HotSpot side:** `java -cp <dir> <Main>` on PATH (JDK 25). No wrapper needed
  because generated programs print their own transcript from `main`. The legacy
  `DiffWrapper__` path from §2.1 is retained only for the value-returning
  micro-method tier.
- **Capture:** stdout, stderr, exit code, and wall-time, with a hard per-run
  timeout (default 120 s like `intrinsic_diff.rs`; configurable). A CratonVM
  timeout while HotSpot finishes is itself a divergence class ("hang"), which
  is how the ES/Tomcat/Hibernate hangs in `MEMORY.md` would surface.

```text
Observation { stdout, stderr, exit_code, exception: Option<JvmException>,
              timed_out: bool, wall_ms: u64 }
JvmException { fqcn: String, message: String, top_frames: Vec<String> }
```

### 3.3 Diff oracle (the comparison + normalization)

The oracle compares two `Observation`s and classifies, with **per-channel
normalization** so we catch real divergences without drowning in benign noise:

- **Exit code:** exact match. (HotSpot maps an uncaught exception in `main` to
  exit `1`; `System.exit(n)` to `n`. CratonVM must agree.)
- **Uncaught exception identity:** parse the standard `Exception in thread
  "main" <fqcn>: <message>` line from stderr on both sides; compare **fqcn**
  (exact) and **message** (exact after normalization). This is the channel that
  catches JEP-358 message parity, CCE type mismatches (`Integer→Character`),
  and reversed stacktrace order (compare the *ordered* `top_frames`).
- **stdout:** exact match after normalization. Normalizers (opt-in per seed via
  a header pragma, default strict): strip absolute paths, mask
  `0x<hex>`/identity hashes (`Object.toString` `@<hash>`), mask thread ids,
  and — for explicitly-tagged nondeterministic programs — sort lines. The
  **default is strict equality**; a seed must *declare* it needs a normalizer,
  so we never silently hide a real diff.
- **stderr (non-exception):** compared loosely (warnings differ legitimately);
  used as a tiebreaker / context, not a hard gate by default.

**Determinism filter (pre-flight):** before a program enters the corpus, run it
**twice on HotSpot**; if the two HotSpot runs disagree under the chosen
normalizer, the program is inherently nondeterministic and is **rejected** (or
flagged normalizer-required). This keeps the oracle sound: any CratonVM≠HotSpot
diff on an accepted program is a real bug, not a coin flip. This is the single
most important correctness property of the harness.

**Classification** (written into the ledger): `JitOnly` (jit-on diverges,
`--nojit` agrees), `GcMode` (only a moving/selective mode diverges), `Universal`
(all CratonVM modes diverge equally → interpreter/native gap), `Hang`
(CratonVM times out), `Crash` (CratonVM exits with a signal/abort while HotSpot
exits cleanly). This classification *is* the triage the human currently does by
hand.

### 3.4 Minimizer

On a confirmed divergence, shrink to a minimal reproducer that **still
diverges** (re-confirmed each step):

- **Source tier:** delphi-style / ddmin line- and statement-level deletion on
  the generated `.java` (drop a statement, re-`javac`, re-run both VMs, keep if
  still divergent), then token-level. Reuses the well-known interestingness-test
  loop: "still compiles AND still diverges."
- **Bytecode tier:** `cargo fuzz tmin` (already documented in `fuzz/README.md`)
  for the libFuzzer mutator, with the interestingness predicate swapped from
  "panics" to "diverges from HotSpot."

The minimized artifact (source or `.class` + the observed `Observation` pair) is
written to `difftest/regression/<short-name>/` and referenced by ledger id.

### 3.5 Divergence ledger + CI gate

Promote `bench/differential-divergences.json` (already produced by §2.1) to a
**committed known-divergence ledger** with the same JSON-baseline discipline as
`bench/hotspot-baseline.json` (schema_version, host, captured_at). Each entry:
`{ id, class, repro_path, classification, cratonvm: Observation,
hotspot: Observation, status: known|fixed|new, first_seen, linked_doc }`.

A `difftest gate` subcommand (modeled on `bench_hotspot_compare.rs`'s exit-code
contract) runs the corpus and:
- exit `0` — no **new** divergences (known ones in the ledger are allowed,
  matching `MEMORY.md`'s reality that many gaps are tracked-but-open).
- exit `1` — a new divergence appeared (regression), or a `known` entry's
  CratonVM side *changed* (silent behavior drift).
- exit `2` — a `fixed` entry diverged again (true regression of a closed bug).
- exit `3` — `java` not on PATH / corpus empty (bootstrap, non-fatal in CI
  smoke).

This makes the loop **CI-enforceable** without requiring 100% HotSpot parity on
day one — the ledger encodes "what we already know is broken," and the gate only
fails on *new* or *regressed* divergence. That is the only realistic gating
posture for a VM that is, by `MEMORY.md`'s own account, ~90% there on many
suites.

### 3.6 Plug-in to existing tooling

- **Reuse `vm/tests/differential.rs`'s** `Outcome`/`Divergence`/`DivergenceReport`
  types — lift them into the `difftest` crate so both the integration test and
  the fuzzer binary share one ledger format and one writer.
- **Reuse `capture-hotspot-baseline.{sh,ps1}`'s** host-tag + JDK-version-guard
  conventions for the ledger header (require JDK 25 unless
  `--allow-jdk-downgrade`).
- **Macro tier** drives `scripts/app-checker.sh` programs as oversized seeds
  (rc + first-error diff against `java`), reusing `triage.sh` classification.
- **CI:** keep the `difftest gate` step in `.github/workflows/ci.yml`
  advisory until the ledger is stable on hosted runners, then remove
  `continue-on-error` to make it blocking.

---

## 4. Incremental delivery plan (small, independently mergeable, each build-green)

Each step compiles and ships value alone; no step requires a later one.

**Step 0 — Scaffolding (see §6).** New `difftest/` workspace crate with a
`difftest` bin that has subcommands `run` / `gen` / `min` / `gate`, all
stubbed to a clean "not yet implemented, here's the plan" exit. Lift the
`Outcome`/`Divergence`/`DivergenceReport`/`JvmException` types from
`vm/tests/differential.rs` into `difftest::ledger`. Config knobs + env flags
declared (no behavior yet). *Green:* crate builds, `difftest --help` works,
`gate` on an empty corpus exits 3. **No source-code behavior change to the VM.**

**Step 1 — Robust two-VM runner over hand seeds.** Implement §3.2 `Runner`
(subprocess both sides, capture stdout/stderr/rc/timeout) and §3.3 oracle for
the four channels (exit code, exception fqcn+message, stdout, stderr), with
strict-by-default normalization. Wire the existing
`vm/tests/resources/cratonvm/*.java` + a dozen `difftest/seeds/` as the first
corpus. Port `diff_basic_arithmetic` / `diff_string_operations` to drive the
new runner. *Green:* `difftest run --corpus seeds` reproduces the manual loop
for ~20 seeds and writes the ledger; `cargo test -p cratonvm-difftest -- --ignored` runs
it. **Immediately useful** — replaces the eyeball loop for the seed set.

**Step 2 — Mode matrix + auto-classification.** Add the per-mode subprocess
fan-out (`jit-on`, `--nojit`, `DISABLE_INTRINSICS`, GC modes, low
`JIT_THRESHOLD`) and the `JitOnly`/`GcMode`/`Universal`/`Hang`/`Crash`
classifier (§3.3). *Green:* a seeded known JIT-only divergence (pick one from
`bench/` that's `--nojit`-clean) is auto-labeled `JitOnly`. This alone
automates the most common manual triage in `MEMORY.md`.

**Step 3 — Determinism filter + ledger gate + CI.** Add the twice-on-HotSpot
determinism pre-flight (§3.3), promote `bench/differential-divergences.json` to
the committed ledger with `known|fixed|new` status, implement `difftest gate`
with the §3.5 exit codes, and add the CI step. *Green:* CI runs the seed corpus
on every PR; new divergences fail the difftest job, and the job becomes
blocking only after `continue-on-error` is removed.

**Step 4 — Grammar-based source generator.** Implement §3.1 tier 2 (typed
self-printing Java generator) behind `difftest gen --grammar`, seeded weighted
toward the bug history. *Green:* `difftest gen | difftest run` finds and
minimizes at least one divergence (or proves parity over N programs) for the
**first target families** below.

**Step 5 — Bytecode mutator (libFuzzer tier).** Add a structured `ClassFile`
mutator as a `fuzz/`-style target whose *interestingness* is "diverges from
HotSpot," reusing `cargo fuzz tmin` for minimization and the §3.4 predicate.
Re-verify each mutant with `classloading`'s verifier before running. *Green:*
the mutator runs under `cargo +nightly fuzz run difftest_bytecode` and promotes
divergent inputs into the differential tier.

**Step 6 — Source minimizer + regression corpus.** Implement §3.4 ddmin source
shrinker; every confirmed divergence lands a minimized repro under
`difftest/regression/` and a ledger entry. *Green:* a reproduced historical bug
(e.g. an arithmetic-overflow or NPE-message case) is minimized to <15 lines and
committed as a regression.

**Step 7 — Macro tier + OSS-Fuzz onboarding.** Wire `app-checker.sh` programs
as macro seeds (rc/first-error diff) and add the `difftest_bytecode` target to
the `fuzz/` OSS-Fuzz `build.sh` sketch already in `fuzz/README.md`.

### First targets (the corners that dominate the bug history)

Ordered by historical bug density in `MEMORY.md`:

1. **`invokedynamic` family** — lambdas, `String` concat indy, record
   `toString`/components, switch patterns, `MethodHandle` invoke/adapt
   (Jackson-3 record deser, `reference_record_generics_jackson3`).
2. **Reflection / generics** — `getDeclaredMethod`, `getGenericSuperclass`,
   bridge methods, `Class.getModifiers` on primitive/array/void
   (`reference_bug06_*`, kafka bug-09 Mockito), `forLanguageTag` CCE.
3. **JIT correctness** — escape-analysis scalar replacement (kafka bug-25),
   catch-bypass (bug-H/I/#13), null-check-elim, inline-cache dispatch
   (bug-24) — surfaced as `JitOnly` via the mode matrix.
4. **GC value-correctness** — bt-style allocate/retain/checksum programs where
   a lost root changes the printed checksum (`project_precise_jit_stack_maps`,
   `default-moving-young-gen`).
5. **Exception semantics** — JEP-358 NPE message parity, stacktrace order,
   try/catch/finally ordering, exit-code mapping.
6. **Arithmetic edge cases** — overflow, `MIN_VALUE/-1`, shift masking, FP
   `NaN`/`-0.0`/`Math.*Exact` overflow strings (cheap, high-yield, the §2.3 FP
   diff already proves the axis).

---

## 5. Risks & open questions

- **Nondeterminism is the central threat.** If an accepted program is secretly
  nondeterministic, the gate flaps. Mitigation: the twice-on-HotSpot
  determinism filter (§3.3) is **mandatory** before corpus admission; default
  to strict equality; require explicit per-seed normalizer pragmas. Open: how
  to handle legitimately-nondeterministic-but-still-diffable cases (e.g.
  HashMap iteration order) — start by *excluding* them, revisit with
  multiset/canonicalized comparison only if a real bug demands it.
- **Generator validity vs interestingness.** A naive generator mostly emits
  programs that both VMs handle trivially (low yield) or that don't compile.
  Mitigation: type-directed generation (always compilable) + weight toward the
  bug-history grammar; measure yield (divergences per 1k programs) and steer.
- **Speed.** Forking `java` + `javac` per program is slow (JVM startup ~hundreds
  of ms; `intrinsic_diff.rs` already eats this). Mitigations: compile once / run
  the same `.class` on both VMs (no per-run `javac`); batch generation; keep the
  libFuzzer in-proc panic tier hot and only promote interesting inputs to the
  slow differential tier; cache HotSpot observations keyed by program hash.
- **Process-lifetime env cache (§2.4).** Each behavioral mode *must* be a fresh
  subprocess; an in-process runner (as §2.1 uses today) cannot vary
  `CRATONVM_DISABLE_JIT` etc. The design commits to subprocess-only for the
  mode matrix — confirmed by `intrinsic_diff.rs`'s own rationale.
- **HotSpot/JDK drift.** The oracle is only as stable as the pinned JDK. Pin to
  JDK 25 (matching `MEMORY.md`'s `C:\Program Files\Java\jdk-25` and the
  baseline scripts' `--allow-jdk-downgrade` guard); record the JDK build in the
  ledger header; re-baseline on JDK bumps.
- **Message-text brittleness.** Exact exception-message matching is exactly what
  we *want* for JEP-358 parity, but JDK patch releases occasionally reword
  messages. Mitigation: the ledger header pins the JDK; message normalizers are
  per-channel and auditable.
- **Windows/Unix path & newline skew.** `differential.rs` already trims
  `\r\n`/`\n`; the runner must normalize line endings and path separators (the
  classpath separator differs — `;` vs `:`, already handled in
  `differential.rs`).
- **Scope creep into multi-threading.** Concurrency divergences (the ES/FJP
  hangs in `MEMORY.md`) are real but the *output* is timing-dependent. Keep the
  generator single-threaded initially; treat hangs as a coarse `Hang`
  classification only.

Open questions: (1) Do we host the generator as Rust (faster, no extra dep) or
shell out to a known JVM fuzzer? Lean Rust for the typed micro-generator,
libFuzzer for bytecode. (2) Should the ledger live in `bench/` (gitignored
`docs/`? no — `bench/` is committed) or a new `difftest/ledger.json`? Lean
`bench/differential-divergences.json` for continuity with §2.1. (3) How aggressive
should the CI smoke budget be (per-target seconds)?

---

## 6. Scaffolding to land first (minimal compiling stubs/flags/knobs)

The first PR (Step 0) should be **all scaffold, no risky logic**, so it builds
green and the rest can land incrementally. Described here, not implemented in
this design doc:

1. **New crate `difftest/`** — a *normal* workspace member (unlike `fuzz/`):
   `difftest/Cargo.toml` (`license.workspace`, `edition.workspace`,
   `lints.workspace`, deps on `cratonvm-vm`, `cratonvm-reader`,
   `cratonvm-classloading`, `serde`/`serde_json`), `difftest/src/lib.rs` with
   modules `ledger`, `runner`, `oracle`, `generate`, `minimize` — each a
   documented stub with the `pub` types from §3.2/§3.3 and `todo!()`-free
   no-op bodies (return `Unimplemented` / empty `Vec`, never panic).

2. **`difftest` binary** (`difftest/src/main.rs`, `[[bin]] name = "difftest"`)
   with clap subcommands `run`, `gen`, `min`, `gate`, each parsing args and
   printing a "planned, not yet wired" message with the §3.5 exit-code contract
   already in place (so CI can adopt the gate step from day one and it just
   passes on an empty corpus).

3. **Lift shared types** — move `Outcome`, `Divergence`, `DivergenceReport`
   (today private to `vm/tests/differential.rs`) into `difftest::ledger`, plus
   the new `Observation`, `JvmException`, `Classification`, and `LedgerEntry
   { id, status: Known|Fixed|New, ... }`. Re-export so the existing integration
   test can switch to the shared types without behavior change.

4. **Binary resolution helper** — copy `intrinsic_diff.rs`'s `cratonvm_binary()`
   (env `CRATONVM_BIN` → `target/release` → `target/debug`) and a
   `java_executable()`/`javac_executable()` pair (already in `differential.rs`)
   into `difftest::runner`, shared by all subcommands.

5. **Config knobs / flags** (declared, default-inert):
   - CLI: `--corpus DIR`, `--modes jit-on,nojit,no-intrinsics,moving-gc`,
     `--timeout-secs` (default 120, matching `intrinsic_diff.rs`),
     `--jdk PATH`, `--allow-jdk-downgrade`, `--ledger FILE`
     (default `bench/differential-divergences.json`), `--update-ledger`.
   - Env: reuse the existing `CRATONVM_BIN`; add `DIFFTEST_JAVA_HOME` (fallback
     to PATH). **No new `CRATONVM_*` VM flags** — the runner only *sets*
     existing ones (`CRATONVM_DISABLE_JIT`, `CRATONVM_DISABLE_INTRINSICS`,
     `CRATONVM_JIT_THRESHOLD`, the GC knobs) per mode; it does not require any
     VM source change.

6. **Corpus directories** — `difftest/seeds/` (a handful of self-printing
   `.java`), `difftest/corpus/.gitkeep`, `difftest/regression/.gitkeep`, and a
   `difftest/README.md` documenting the run/min/gate workflow (mirroring
   `fuzz/README.md`'s structure, which the review wants but `fuzz/` lacks).

7. **CI integration** — an advisory `difftest gate` step in
   `.github/workflows/ci.yml` using `continue-on-error` until Step 3 is stable
   enough to enforce on hosted runners.

**Explicitly not in Step 0:** no generator logic, no mutator, no real diffing
beyond compiling the types, **no edits to any VM source crate** (the whole
feature is additive tooling that *drives* the existing `cratonvm` binary and
`java`). This keeps the keystone risk at zero and lets each later step be a
small, reviewable, build-green PR.

---

## 7. Validation / acceptance (how to prove it works)

- **Bootstrapping proof (Step 1):** point the runner at the existing
  `vm/tests/resources/cratonvm/DiffArithmetic` + `DiffString` and the `bench/`
  programs; it must reproduce the *same* match/divergence verdicts that
  `vm/tests/differential.rs` produces today (regression against the known-good
  manual harness).
- **Known-bug replay (Steps 4–6):** seed the corpus with minimized repros of
  *already-fixed* bugs from `MEMORY.md` (e.g. an arithmetic-overflow case, a
  JEP-358 NPE-message case, a `--nojit`-clean JIT case). On the **fixed** VM the
  gate is green; checked out *before* the fix, the fuzzer must independently
  **rediscover and minimize** the divergence — proving the harness can find
  real bugs, not just confirm parity.
- **GC value-correctness anchor:** run a bt-style allocate/retain/checksum seed;
  CratonVM (default, selective-promote) must print `68332206` to match
  `java -cp bench BenchSuite` (the canonical HotSpot checksum in `MEMORY.md`).
  A moving-GC mode that under-counts shows up as a `GcMode` divergence — proving
  the mode matrix catches GC correctness, not just throughput.
- **Auto-classification check (Step 2):** for a curated JIT-only case, the
  classifier must emit `JitOnly` (jit-on diverges, `--nojit` agrees) without
  human input — validating the triage automation.
- **Determinism-filter check (Step 3):** feed a deliberately nondeterministic
  program (identity-hash `toString`, HashMap iteration order); the pre-flight
  must **reject** it (two HotSpot runs disagree) rather than admit a flaky
  ledger entry.
- **CI gate check (Step 3):** a synthetic "regress one seed" branch must turn
  the `difftest gate` job red (exit 1); the baseline branch stays green
  (exit 0). While advisory, this reports drift without blocking the whole PR.
- **No-regression guard:** the new crate must not perturb VM behavior — `bt18`
  still `68332206` and the existing differential/intrinsic_diff tests still pass
  (the feature is additive tooling; it touches no VM source crate).

The acceptance bar is **not** "100% HotSpot parity" (unrealistic per
`MEMORY.md`) but: *the harness reproduces the manual loop, auto-classifies
JIT/GC/universal divergences, rediscovers known bugs from a pre-fix checkout,
and fails CI only on new or regressed divergence.*
