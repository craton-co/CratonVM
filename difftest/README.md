# difftest — semantic differential fuzzer vs HotSpot

Runs Java programs on **both** CratonVM and a real JDK and **diffs observable
behavior** — stdout, stderr, thrown exception type + message, and process exit
code — automatically. This industrializes the manual "run it on HotSpot and
eyeball the diff" loop that produced nearly every bug in `docs/internal/*` and
`MEMORY.md`.

Full design: [`docs/feature-designs/differential-fuzzer.md`](../docs/feature-designs/differential-fuzzer.md).

> **Status: Step 3 — determinism pre-flight + ledger gate.** `run` A/Bs the
> corpus across the CratonVM mode matrix vs HotSpot and auto-classifies each
> divergence; `gate` adds the twice-on-HotSpot determinism filter, divergence
> re-confirmation, and the committed-ledger verdict (the §3.5 exit codes).
> `gen` / `min` remain stubs (Steps 4 / 6). Each step (design doc §4) is a
> small, independently mergeable, build-green PR.

## Layout

```
difftest/
  src/
    ledger.rs    divergence records + the committed JSON ledger (§3.5)
    runner.rs    two-VM A/B executor: binary resolution + mode matrix (§3.2)
    oracle.rs    per-channel compare + normalize + classify (§3.3)
    harness.rs   compile + run the matrix + diff + gate (§3.5)
    generate.rs  corpus generator (§3.1)        [stub → Step 4]
    minimize.rs  reproducer shrinker (§3.4)      [stub → Step 6]
    main.rs      the `difftest` CLI
  seeds/         curated self-printing .java programs (the first corpus)
  ledger.json    committed known-divergence ledger (the gate's baseline)
  corpus/        live corpus (generated / promoted inputs)   [grows at runtime]
  regression/    minimized, committed repros of confirmed divergences
```

## Usage

```bash
# Build the crate and the cratonvm binary the runner drives.
cargo build -p cratonvm-difftest
cargo build -p cratonvm-cli            # produces target/debug/cratonvm

# A/B the corpus (Step 1+). Step 0 prints its plan.
difftest run --corpus difftest/seeds --modes jit-on,nojit

# Generate programs biased toward the bug history (Step 4).
difftest gen --count 500 --out difftest/corpus

# Minimize a confirmed-divergent program (Step 6).
difftest min difftest/corpus/Found_0042.java

# CI gate: exit non-zero only on a *new* or *regressed* divergence (Step 3).
difftest gate --corpus difftest/seeds
```

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
- **JDK:** `--jdk <home>` → `DIFFTEST_JAVA_HOME` → `java` on PATH. Pin to JDK 25
  to match the `MEMORY.md` baseline; `--allow-jdk-downgrade` relaxes the pin.

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
