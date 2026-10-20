# difftest — semantic differential fuzzer vs HotSpot

Runs Java programs on **both** CratonVM and a real JDK — and through each of
CratonVM's own execution paths — and **diffs observable behavior** across eight
independently-reported dimensions: exit status, uncaught-exception presence /
type / message / frames, stdout, stderr, and a program-declared checksum. This
industrializes the manual "run it on HotSpot and eyeball the diff" loop that
produced nearly every bug found and fixed during this project's manual bug-hunt
history.

Each dimension is judged separately, so a divergence **names the observable that
moved** rather than saying two runs differ; and every transform applied before a
comparison is a named rule with a documented justification and a documented
risk, so an over-normalization is reviewable rather than buried in a regex.

Full design: [`docs/feature-designs/differential-fuzzer.md`](../docs/feature-designs/differential-fuzzer.md).
Operator guide — every dimension, every normalization rule and its risk, the
execution-path mode axis, how to read the coverage matrix, and how to tell a
harness false positive from a real VM divergence:
[`docs/testing/differential.md`](../docs/testing/differential.md).

> **Status: Steps 0-7 wired.** `run` A/Bs the corpus across the CratonVM mode
> matrix vs HotSpot and auto-classifies each divergence; `gate` adds the
> twice-on-HotSpot determinism filter, divergence re-confirmation, and the
> committed-ledger verdict (the exit-code contract below). `gen`, `mutate`, and
> `min` provide the corpus-growth and reproducer workflows.

## Layout

```
difftest/
  src/
    ledger.rs    divergence records + the committed JSON ledger (§3.5)
    runner.rs    two-VM A/B executor: binary resolution + mode matrix (§3.2)
    oracle.rs    per-dimension compare + normalize + classify (§3.3)
    normalize.rs the named normalization rules (target/justification/risk)
    checksum.rs  the program-declared checksum dimension
    crossmode.rs CratonVM path-vs-path comparison (needs no reference JDK)
    matrix.rs    opcode / execution-path coverage matrix, derived from the
                 corpus class files themselves
    opcorpus.rs  the generated opcode corpus: one deterministic, checksum-
                 declaring Java program per JVMS opcode (197 of 202)
    harness.rs   compile + run the matrix + diff + gate (§3.5)
    generate.rs  corpus generator (§3.1)
    minimize.rs  reproducer shrinker (§3.4)
    census.rs    reads the launcher's JDK-only dumps (jdk-only-mode.md §9)
    main.rs      the `cratonvm-difftest` CLI
  seeds/         curated self-printing .java programs (the first corpus)
  seeds-jdk-only/ JDK-only boundary vectors — a SIBLING of seeds/, not a child,
                 because `discover_programs` is non-recursive and these are
                 expected to diverge until §5 enforcement lands
  ledger.json    committed known-divergence ledger (the gate's baseline)
  corpus/        live corpus (generated / promoted inputs)   [grows at runtime]
  corpus-opcodes/ generated opcode corpus  [build product, .gitignore'd]
  regression/    minimized, committed repros of confirmed divergences
```

## Usage

```bash
# Build the crate and the cratonvm binary the runner drives.
cargo build -p cratonvm-difftest
cargo build -p cratonvm-cli            # produces target/debug/cratonvm

# A/B the corpus.
cratonvm-difftest run --corpus difftest/seeds --modes jit-on,nojit

# Generate programs biased toward the bug history (Step 4).
cratonvm-difftest gen --family arith --count 500 --seed 1 --out difftest/corpus
cratonvm-difftest run --corpus difftest/corpus      # then A/B them

# Mutate a compiled seed's constant pool and A/B each mutant (Step 5).
cratonvm-difftest mutate difftest/seeds/StringConcatIndy.java --count 50 --seed 1
# Panic-fuzz the mutator in-process (needs nightly + cargo install cargo-fuzz):
cargo +nightly fuzz run difftest_bytecode   # from the fuzz/ dir

# Minimize a confirmed-divergent program (Step 6).
cratonvm-difftest min difftest/corpus/Found_0042.java

# CI gate: exit non-zero only on a *new* or *regressed* divergence (Step 3).
cratonvm-difftest gate --corpus difftest/seeds

# JDK-only mode: measure the strict policy against the compatible one.
cratonvm-difftest gate --corpus difftest/seeds-jdk-only \
    --modes jdk-only-jit,jdk-only-nojit,real-compatible-jit

# The execution-path contract: run the same program through every one of the
# VM's semantic implementations and cross-compare them (C2 review P1).
cratonvm-difftest run --corpus difftest/seeds \
    --modes nojit,interp-decoded,direct-emit,ir-jit,osr-eager,no-osr,forced-deopt

# Generate the opcode / execution-path coverage matrix (C2 review P0).
cratonvm-difftest matrix --corpus difftest/seeds --show-gaps

# ...but a matrix over the 3-program seed corpus measures almost nothing, so
# generate one program per opcode first and point the matrix at that.
cratonvm-difftest gen-opcodes --out target/difftest-opcodes
cratonvm-difftest matrix --corpus target/difftest-opcodes --show-gaps
```

### The generated opcode corpus

`gen-opcodes` emits **one Java program per JVMS opcode** — 197 of 202, with the
five `javac` cannot produce (`nop`, `swap`, `jsr`, `ret`, `jsr_w`) reported as
`unreachable-by-construction` with the reason rather than as gaps anybody could
close. Each program is deterministic, declares a `##DIFFTEST-CHECKSUM##` line
(before this, *no* seed did, so the fifth comparison dimension was never
exercised), and wraps its focus opcode in a loop **and** a handler so the `osr`
and `exception` axes have code sites at all — those facts are per method, so
helper methods carry their own pair.

Three recipes exist only because generation makes them practical: `ldc_w` needs a
constant pool deeper than 255 entries, `wide` needs more than 255 locals, and
`goto_w` needs a method body over 32 KiB. Nobody writes those by hand.

The corpus is a **build product** (`.gitignore`d): `gen-opcodes` reproduces it
byte-identically, and committing it would let the programs drift from the
recipes that describe them. Full write-up, including what the matrix still does
not claim: [`docs/testing/opcode-coverage.md`](../docs/testing/opcode-coverage.md).

### Execution-path modes

CratonVM has **four semantic implementations** — the interpreter's raw fast path
with superinstructions, the interpreter's decoded fallback, the single-pass
direct x64 emitter, and the optimizing IR pipeline — plus OSR and deopt as
transitions between them. A fix that lands in one and not the others is a
wrong-code risk, so six modes drive them explicitly:

| Mode | Selects | Flags (all verified against live read sites) |
|------|---------|---------------------------------------------|
| `nojit` | interpreter, raw/superinstruction handlers | `CRATONVM_DISABLE_JIT=1` |
| `interp-decoded` | interpreter, decoded fallback | `--noverify` + `CRATONVM_DISABLE_JIT=1` |
| `direct-emit` | single-pass x64 emitter (approximate) | `CRATONVM_NO_IR_BRANCHY=1`, `CRATONVM_JIT_IR_CALL=0` |
| `ir-jit` | optimizing IR pipeline | `CRATONVM_JIT_FORCE_C2=1` |
| `osr-eager` / `no-osr` | back-edge OSR on (first back-edge) / off | `CRATONVM_JIT_OSR`, `CRATONVM_TIER_OSR_BACKEDGE=1` |
| `forced-deopt` | compiled → interpreted transition | `CRATONVM_DEOPT_EAGER=1`, `CRATONVM_DEOPT_REAL=1`, `CRATONVM_DEOPT_VERIFY=1` |

A divergence **between two of these** is stronger evidence than a divergence
against HotSpot — it needs no reference, so neither the reference nor the
normalization can be blamed for it. Such splits are printed as `PATH` lines and
are deliberately **not** gated (a `known` `jit-only` ledger row is a path split
by construction). Caveats — in particular that `interp-decoded` also turns off
bytecode verification, and that `direct-emit` is approximate because the VM
exposes no hard "IR off" switch — are in
[`docs/testing/differential.md`](../docs/testing/differential.md).

### Gating the execution paths, and fuzzing them

Two subcommands **gate** those splits, with the interpreter as the oracle and
no HotSpot run or ledger row involved
([`docs/testing/jit-differential.md`](../docs/testing/jit-differential.md)):

```bash
# Every execution-path mode (+ moving-gc) must agree with nojit. CI: difftest-jit-paths.
cratonvm-difftest path-gate --corpus difftest/seeds

# Generated class files, written directly as bytecode, JIT modes vs nojit.
cratonvm-difftest fuzz-jit --seed 1 --count 100
cratonvm-difftest fuzz-jit --seed 1 --count 100 --extra-env CRATONVM_DBG_GC_STRESS=65536
cratonvm-difftest fuzz-jit --dry-run --seed 1 --count 256      # generator checks only, no VM
```

Tracked splits live in `difftest/path-gate-known.json`. Failing fuzz programs
are minimized to a runnable `.class` under `target/jitfuzz-failures/`.

### JDK-only modes

`docs/feature-designs/jdk-only-mode.md` adds a second axis to the mode matrix:
the **compatibility policy** the launcher runs under.

| Mode | Launcher flag | `jdk_profile` | Census dumps |
|------|---------------|---------------|--------------|
| `jit-on`, `nojit`, `no-intrinsics`, `moving-gc`, `low-jit-threshold` | *(none)* | `compatible` | no |
| `jdk-only-jit`, `jdk-only-nojit` | `--jdk-only` | `jdk-only` | yes |
| `real-compatible-jit`, `real-compatible-nojit` | `--real-jdk` | `compatible` | yes |

The five historical modes are **frozen** — same labels, same argv, same env, no
dump flags — so `gate --corpus difftest/seeds` keeps measuring exactly what it
measured before, and `--modes` still defaults to `jit-on,nojit`.

A ledger row is keyed by **`(class, jdk_profile)`**. The same program diverging
under both policies produces two rows (`Foo` and `Foo@jdk-only` in gate output),
judged independently: a `known` compatible divergence can never excuse a strict
one. There is deliberately **no fallback** — a divergence is re-confirmed by
re-running the *same* mode, and a `--jdk-only` child that reports the compatible
profile is recorded as a `profile-mismatch` violation, which is how a silent
VM-side downgrade is caught.

Wave 1 is **measurement, not enforcement** (contract §10). Recorded violations
are listed in the gate report but do **not** move its exit code; a violation
that actually changed behaviour is already a divergence and gates as one.

### Gate exit codes (design §3.5)

| Code | Meaning |
|------|---------|
| `0`  | No **new** divergences (known ledger entries are allowed). |
| `1`  | A new divergence appeared, or a `known` entry's CratonVM side changed. |
| `2`  | A `fixed` entry diverged again — a true regression of a closed bug. |
| `3`  | Bootstrap / non-fatal: `java` unavailable or the corpus is empty. |

## Binary & JDK resolution

- **CratonVM:** `CRATONVM_BIN` → `target/release/cratonvm[.exe]` →
  `target/debug/cratonvm[.exe]` (same order as `vm/tests/intrinsic_diff.rs`).
- **JDK:** `--jdk <home>` → `DIFFTEST_JAVA_HOME` → `java` on PATH. The ledger
  records the JDK banner used for capture, and CI provisions JDK 25 for the
  HotSpot oracle, but local `run`/`gate` invocations do not enforce a JDK
  version.

The runner sets only **existing** `CRATONVM_*` env vars per mode
(`CRATONVM_DISABLE_JIT`, `CRATONVM_DISABLE_INTRINSICS`, `CRATONVM_JIT_THRESHOLD`,
the GC knobs) — it requires **no** VM source change and adds no new VM flag.

## Determinism is mandatory

A program enters the corpus only if it is deterministic: before admission it is
run **twice on HotSpot** and rejected if the two runs disagree under the chosen
normalizer (design §3.3). Default comparison is **strict equality**; a seed must
*declare* it needs a normalizer via a `// difftest: <pragma>` header. This keeps
the oracle sound — any CratonVM≠HotSpot diff on an accepted program is a real
bug, not a coin flip.

Every run prints the named normalization rules that were active on its second
line, so a rule that should not be on is visible at the top of the log rather
than in a source file. The rules, and the specific real divergence each one
would hide, are tabulated in
[`docs/testing/differential.md`](../docs/testing/differential.md) §2.

## Tiers

The corpus comes in three tiers of increasing blast radius (design §3.1):

1. **Curated seeds** (`seeds/`) — small, fully-observable, self-printing
   programs; the committed gate corpus.
2. **Generated** (`cratonvm-difftest gen`) — seeded, reproducible, type-directed
   programs; and **mutated** (`cratonvm-difftest mutate`) — constant-pool perturbations
   of a compiled seed.
3. **Macro tier** — whole real programs as oversized seeds. Point
   `cratonvm-difftest run` at any directory of `.java`/`.class` (e.g. `bench/*.java`, or programs
   under `apps/`); the same four-channel oracle applies, with the per-run
   timeout turning a CratonVM hang into a `Hang` divergence. Because real apps
   are often nondeterministic (threads, wall-clock, hashmap order), run the
   macro tier with `--check-determinism` so the twice-on-HotSpot pre-flight
   rejects flaky programs rather than admitting a coin-flip — the macro tier is
   an *rc + first-divergence* signal, not a strict full-transcript gate.

The bytecode tier also has a fast in-process panic fuzzer
(`cargo +nightly fuzz run difftest_bytecode`, see `fuzz/README.md`), which is
onboarded to the OSS-Fuzz `build.sh` sketch there.
