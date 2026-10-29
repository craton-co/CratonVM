# Opcode / execution-path coverage: the generated corpus and the matrix

*Scope: `difftest/src/opcorpus.rs` (the generator) and `difftest/src/matrix.rs`
(the report). Companion to
[`docs/testing/differential.md`](differential.md), which covers the comparison
dimensions, the normalization rules and the mode axis.*

---

## 1. Why this exists

CratonVM does not have one executor. It has **four semantic implementations** —
the interpreter's raw fast path with superinstruction fusion, the interpreter's
decoded fallback, the single-pass x64 emitter, and the optimizing IR pipeline —
plus **OSR** and **deoptimization** as cross-cutting transitions. Every opcode is
therefore implemented four or more times, in four places that can drift.

> A fix that lands in one path and not the others is the wrong-code risk this
> project's history is fullest of. The only way to see it coming is to know,
> per opcode and per path, whether *anything* has ever run that opcode down that
> path.

`cratonvm-difftest matrix` answers that question from the corpus's own class
files. But a report is only as good as the corpus underneath it, and the
committed `difftest/seeds` corpus is three programs exercising roughly four of
the 202 named opcodes. A matrix over that corpus is a precise measurement of
almost nothing.

`cratonvm-difftest gen-opcodes` is the other half: it emits **one Java program
per opcode**, so the matrix has something to measure.

---

## 2. Running it

```bash
# 1. Emit the corpus (197 programs; deterministic, no network, no VM needed).
cargo run -p cratonvm-difftest --bin cratonvm-difftest -- \
    gen-opcodes --out target/difftest-opcodes

# 2. Compile it with the same javac the differential run uses, walk the bytes,
#    and report the grid.
cargo run -p cratonvm-difftest --bin cratonvm-difftest -- \
    matrix --corpus target/difftest-opcodes --show-gaps \
    --out target/coverage-matrix.json

# What the generator plans to emit, without writing anything:
cargo run -p cratonvm-difftest --bin cratonvm-difftest -- gen-opcodes --dry-run
```

The corpus is a **build product** and is `.gitignore`d
(`difftest/.gitignore`). Committing ~200 machine-written programs would put them
in every review diff without adding a fact the generator does not already state,
and would let the two drift — which is the failure the generator exists to
prevent. `gen-opcodes` reproduces them byte-identically.

Running the corpus differentially is a separate, much heavier job (197 programs
× 7 modes × two VMs); `matrix` only compiles.

---

## 3. What every generated program has

Three properties, each load-bearing:

**A back-edge and a handler around the focus code.** The matrix's code-side gate
is per *method*: the `osr` axis needs the opcode to sit in a method with a
backward branch (there is no back-edge to OSR from otherwise), and the
`exception` axis needs a non-empty exception table. Every program therefore runs
its focus code inside a loop inside a `try`, and every generated *helper* method
carries its own loop and handler — the facts do not cross a method boundary.
Without this the whole `osr` and `exception` columns stay empty however many
opcodes the corpus gains. `programs_wrap_their_focus_in_a_loop_and_a_handler`
holds the line.

**A declared checksum.** Every program prints
`##DIFFTEST-CHECKSUM## op<nnn>-<mnemonic> <hex>` — an FNV-1a 64 accumulator
folded over every intermediate the loop produces. Before this corpus, *no seed
declared one*, so the fifth comparison dimension existed and was never exercised.
It is read from un-normalized stdout, so no normalization rule can launder it
(see `docs/testing/differential.md` §3).

**Determinism.** No wall-clock, no `Object.hashCode` (identity-based, and
explicitly unspecified), no map iteration order, no locale-sensitive formatting.
`String.hashCode` and `String.length` are specified; those are used instead. Loop
bounds derive from `args.length` — 0 at run time and opaque to `javac` — so the
trip count survives constant folding and the loop is really executed.

---

## 4. Which opcodes cannot be generated, and why that is stated

Five opcodes have **no Java source that makes `javac` emit them**. They are
reported as `unreachable-by-construction` with the reason attached, never mixed
in with ordinary gaps — an ordinary gap says "write a seed", and for these that
advice is simply wrong.

| Opcode | Why no source produces it | Where it *can* come from |
|---|---|---|
| `nop` (0x00) | No Java construct emits one; it carries no semantics to express | the `mutate` tier, or a hand-assembled class |
| `swap` (0x5f) | `javac`'s code generator never emits it — it reorders the operand stack with `dup_x1`/`dup_x2`/`pop` | the `mutate` tier |
| `jsr` (0xa8), `ret` (0xa9), `jsr_w` (0xc9) | The pre-Java-6 `finally` subroutine. `javac` has not emitted them since 1.5, and **JVMS §4.9.1 forbids them in class files of version ≥ 50.0** | only a hand-assembled version ≤ 49 class file |

Two more are generatable but *large by construction*, and `--no-large` reports
them as skipped-with-a-reason rather than silently covered:

* `wide` (0xc4) needs a method with more than 255 locals, so the generator emits
  300 of them plus one `long`, one `double` and one reference past the boundary —
  giving `wide:iload`, `wide:istore`, `wide:iinc`, `wide:lload`, `wide:dload` and
  `wide:aload`.
* `goto_w` (0xc8) needs a branch offset that does not fit in a signed 16-bit
  field, i.e. a method body over 32 KiB. `javac` regenerates such a method in
  "fat code" mode and widens its branches, so the generator emits a loop whose
  body is ~40 KiB of bytecode (10 000 four-byte statements) — comfortably inside
  `javac`'s 64 KiB per-method limit.

`ldc_w` (0x13) is the third recipe that only generation makes practical: it needs
a constant-pool index past 255, so the program interns 400 distinct string
constants. That is a loop here and would be unreviewable by hand.

**Everything else — 197 of 202 — is generated.**

---

## 5. Reading a cell

Each `(opcode, axis)` cell is one of three states. A boolean could not tell
"nobody wrote a seed" from "nothing *can* write one", and `jsr` is permanently
the second.

| State | Meaning |
|---|---|
| `covered` | a configured mode drives the axis **and** the corpus puts the opcode somewhere the axis reaches |
| `uncovered` | carries a `gap` naming which fix it needs, and the `fix` text itself |
| `unreachable-by-construction` | carries the `reason` |

The gaps, and who owns each:

| `gap` | Fix |
|---|---|
| `no-mode-drives-axis` | add a mode to `--modes` (the whole column is dead) |
| `not-in-corpus-but-generated` | regenerate the corpus — a recipe exists |
| `no-generator` | write a recipe in `difftest/src/opcorpus.rs`, or add a seed |
| `no-loop-site` | the opcode is present but never inside a method with a backward branch |
| `no-handler-site` | present, but never inside a method with a non-empty exception table |

`--show-gaps` prints one line per distinct gap per opcode, with the axes it
applies to, so an opcode missing entirely does not print seven identical lines.

### Precedence, and why corpus evidence wins

1. **Evidence beats declaration.** If the scanned bytes contain the opcode, no
   "unreachable" claim overrides that — a mutated class really can carry a
   `swap`, and a report that called it unreachable while the corpus executed it
   would be lying. The contradiction lands in `reconciliation` instead.
2. Unreachable-by-construction, from the generator's declaration.
3. Not in the corpus, split by whether a generator exists — "regenerate" and
   "write a recipe" are different jobs for different people.
4. No mode drives the axis.
5. Code-side: no loop site (`osr`), no handler site (`exception`).

---

## 6. Reconciliation: what keeps the generator honest

Each generator entry carries a **witness** — the raw encoding of the focus
instruction. It exists so the matrix walker can be tested hermetically (no JDK,
no `javac`, no subprocess) for all 202 opcodes, including the three
variable-length ones.

**The witness is a declaration, not proof that `javac` emitted the opcode.** The
proof is the reconciliation step in `matrix`: it compiles the generated corpus
with the same `javac` the differential run uses, walks the bytes, and reports
every disagreement, in both directions:

```
RECONCILE goto_w (0xc8) — a generator claims this opcode, but the compiled
  corpus does not contain it — the recipe no longer produces it, or this corpus
  was not generated
RECONCILE swap (0x5f) — declared unreachable-by-construction (…) but the scanned
  corpus contains it — the corpus wins; the declaration is stale
```

A `javac` release that changes a lowering shows up there rather than quietly
inflating the coverage number. **`RECONCILE` lines on a freshly generated corpus
are the signal to act on**; they are the one output of this tooling that means a
recipe is broken rather than a gap is open.

Note the deliberate asymmetry with the `gate` subcommand: `matrix` is a report
and always exits 0. A coverage gap is a backlog item, not a build failure, and
failing here would make *adding an axis* a red build.

---

## 7. What the matrix still does not claim

Unchanged from `docs/testing/differential.md` §6, and worth repeating because
the generated corpus makes the numbers look more authoritative than they are:

Running a program under `ir-jit` does **not** prove the IR pipeline compiled the
method containing a given opcode. Admission is a conjunction of six independent
terms inside `jit/src/lib.rs`, any of which can decline silently. The matrix
reports **opportunity** — "the corpus contains this opcode and a mode driving
this path ran it" — not confirmed execution.

**Required VM-side change to close this:** a `--dump-compilation-report <FILE>`
launcher flag alongside the three existing `jdk-only` census dumps in `vm-cli`,
letting the harness assert *which* path actually compiled each method. Out of
scope for the differential harness, which touches no VM crate.

Two smaller caveats inherited from the mode axis:

* `direct-emit` is approximate — the VM exposes no hard "IR off" switch, so
  methods the IR builder can fully build take the IR pipeline regardless.
* `interp-decoded` also disables bytecode verification. Inert for a
  `javac`-produced corpus (these classes verify anyway); **not** inert for the
  `mutate` tier.

---

## 8. CI

**Wired.** Two steps in the `difftest-gate` job of `.github/workflows/ci.yml`,
after the existing gate step — that job already provisions the JDK 25 and the
build both need, and `matrix` must compile with the *same* `javac` the
differential run uses or the grid describes bytes nobody executes.

The generate step is a shell block rather than the two bare `cargo run` lines,
for one reason: GitHub Actions fails a step on any non-zero exit, so a literal
transcription would turn the documented **non-fatal exit 3** into a red build the
first time a runner came up without `javac`. The block accepts 0 and 3, and
passes every other code through unchanged. `if-no-files-found: warn` on the
upload is the same argument on the artifact side — the exit-3 path legitimately
produces no matrix file.

```yaml
      - name: Generate the opcode corpus and the coverage matrix
        shell: bash
        run: |
          set -uo pipefail
          status=0
          cargo run -p cratonvm-difftest --bin cratonvm-difftest -- \
            gen-opcodes --out target/difftest-opcodes || status=$?
          if [ "$status" -eq 3 ]; then
            echo "::notice title=opcode corpus::gen-opcodes bootstrapped (exit 3); nothing to measure."
            exit 0
          elif [ "$status" -ne 0 ]; then
            exit "$status"
          fi
          status=0
          cargo run -p cratonvm-difftest --bin cratonvm-difftest -- \
            matrix --corpus target/difftest-opcodes --show-gaps \
            --out target/coverage-matrix.json || status=$?
          if [ "$status" -eq 3 ]; then
            echo "::notice title=coverage matrix::matrix bootstrapped (exit 3); no corpus or no javac."
            exit 0
          fi
          exit "$status"

      - name: Upload the coverage matrix
        uses: actions/upload-artifact@v4
        with:
          name: opcode-coverage-matrix
          path: target/coverage-matrix.json
          if-no-files-found: warn
```

`matrix` exits 3 (non-fatal) if the corpus is empty or `javac` is unavailable,
and 0 otherwise — including on a corpus full of gaps — so the step is adoptable
before anyone commits to a coverage ratchet. Turning it into a ratchet later
means comparing `cells.covered` against a committed baseline; the schema is
versioned (`schema_version`) precisely so that comparison stays meaningful across
changes.

The corpus is written under `target/`, which the root `.gitignore` already
covers; `difftest/.gitignore` covers the default in-crate locations
(`/corpus-opcodes/`, `/coverage-matrix.json`) for local runs, and
`difftest/Cargo.toml`'s `exclude` keeps `corpus-opcodes/**` out of
`cargo package`.

**What this step does not do:** it compiles the generated corpus and reports the
grid. It does not run it differentially against HotSpot (197 programs × 7 modes ×
two VMs). The `checksum` dimension therefore has programs that declare one, but
no CI job that compares them — see `docs/testing/differential.md` §8.
