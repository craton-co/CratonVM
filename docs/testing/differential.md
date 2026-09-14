# Differential testing: what is compared, and how to trust the answer

*Scope: the `difftest/` crate (`cratonvm-difftest`). Companion to
[`docs/feature-designs/differential-fuzzer.md`](../feature-designs/differential-fuzzer.md)
(the design) and [`difftest/README.md`](../../difftest/README.md) (the CLI).*

This document exists because of one recurring failure mode, recorded in this
project's own history:

> **A differential harness will blame the VM for its own bugs.** A raw diff
> between two processes is not evidence. Environment differences, nondeterminism
> the JLS does not fix, and the harness's own comparison logic all produce
> divergences that look exactly like wrong code — and each one burns a
> bisection.

No CratonVM-vs-HotSpot divergence is currently open. The live record is the
harness itself — `vm/tests/differential.rs` plus the report it writes to
`bench/differential-divergences.json` — not a hand-maintained log. To add a
case, write a fixture class under `vm/tests/resources/cratonvm/`, add an
`#[ignore]`d `assert_main_matches_hotspot("cratonvm/YourFixture")` test, or
point the `DIFFERENTIAL_CLASSES` env var at it for an ad-hoc run.

Everything below is organised around making a reported divergence *actionable*:
you should be able to read a report and say which observable moved, which
transforms stood between the two runs, and whether a reference JDK was involved
at all.

---

## 1. What is compared

Eight independent **dimensions**. Each is judged separately and contributes its
own entry to a divergence, so a report names the observable that moved rather
than saying "these two runs differ". Implementation: `difftest/src/oracle.rs`
(`compare`), dimension names: `difftest/src/ledger.rs` (`Channel`).

| Dimension | Label | Discipline |
|---|---|---|
| Process exit status | `exit-code` | Exact. A CratonVM **timeout** renders as `<timeout>` and a signal kill as `<signal>`, so neither can ever compare equal to a clean exit. |
| Uncaught exception **presence** | `exception` | One side threw and the other did not. Short-circuits: the three dimensions below are not also reported against `<none>`, so one finding counts once. |
| Uncaught exception **type** | `exception-type` | `fqcn`, exact, **never normalized**. A class name is a semantic observable; no rule has any business rewriting one. |
| Uncaught exception **message** | `exception-message` | Exact after normalization. |
| Uncaught exception **frames** | `exception-frames` | The `at …` lines compared **in order**, one per line, each normalized. Order matters so a reversed stack trace is a finding, not a wash. |
| Program stdout | `stdout` | Exact after normalization. |
| Program stderr | `stderr` | Gated **only** under the `jdk-only` / `real-compatible` profile normalizer, and only after the VM's own diagnostics are removed. CratonVM's warnings legitimately differ from HotSpot's silence; gating them unconditionally would turn the whole corpus red. |
| Program-computed **checksum** | `checksum` | The quantities the program itself declares, read from **un-normalized** stdout. See §3. |

### Why the exception dimension is four dimensions

"The exception differed" is not actionable. A wrong **type** is a dispatch or
resolution bug; a wrong **message** is usually a formatting or helpful-NPE gap;
wrong **frames** are an attribution bug in the unwinder. The committed
`ExceptionId` ledger row is precisely the middle case — right type, right frames,
a `ClassCastException` message missing HotSpot's module/loader detail — and
collapsing all three into one rendered string made that read as an opaque
"exception" diff.

---

## 2. Normalization: named rules, never a buried regex

Every transform the oracle may apply is a `NormalizationRule` in
`difftest/src/normalize.rs`, carrying four fields a reviewer can argue with:
`target` (exactly what it neutralizes), `justification` (why that is not a
semantic observable), `risk` (**the real divergence it would hide**), and a
`probe` (a sample it must rewrite).

Two tests enforce the contract:

* `every_rule_neutralizes_its_own_probe` — the rule actually hits its target;
* `no_rule_touches_another_rules_probe` — and hits **only** its target.

**Only `line-endings` is on by default.** `Normalizer::strict()` selects nothing
else, so the committed `difftest/seeds` gate baseline still judges byte-exact
output. A seed opts in per rule.

| Rule | Target | Why it is not a semantic observable | Risk it carries |
|---|---|---|---|
| `line-endings` | CRLF terminators, trailing whitespace at end of stream | The line terminator is the C runtime's text mode, not the program's choice | A deliberate trailing space or final bare `\r` becomes invisible |
| `identity-hash` | `@<hex>` with ≥ 4 hex digits | `System.identityHashCode` is explicitly unspecified; HotSpot uses a thread-local PRNG, CratonVM the object address | Text legitimately shaped `name@hexdigits` is masked. Hence the ≥ 4-digit and non-identifier-terminator guards |
| `hex-address` | `0x<hex>` not glued to an identifier | Raw addresses (Unsafe, DirectByteBuffer, JNI handles) can never agree between two allocators | A program whose *result* is a hex string is masked — print digests through the checksum channel instead |
| `thread-id` | `Thread-<n>`, `tid=<n>`, `pid=<n>` | Thread numbering counts runtime-internal threads started before `main`, which differ; pids are per-run by definition | A program asserting on default thread names loses the assertion |
| `timestamp` | ISO-8601 date, optional `T`/space + time + fraction + `Z` | The reference run and the VM run happen at different instants, by construction | **`java.time` formatting is a live bug area for this VM.** Never enable on a formatting seed |
| `path-separator` | `\` inside a path-shaped token | The reference JDK may run on a Linux worker while CratonVM runs on Windows; a committed ledger row must be readable on both | A literal backslash in a token that also contains `/` (a Windows-flavoured regex) is rewritten |
| `absolute-path` | An absolute path prefix up to its last separator; basename kept | The harness compiles into a pid-namespaced temp dir and the VMs are invoked from different roots | A path-resolution bug producing the *wrong directory* but the right file name becomes invisible — do not enable on `getCanonicalPath` / `Path.normalize` seeds |
| `frame-line-numbers` | `.java:<digits>` inside frames | Line numbers come from the `LineNumberTable` the reference JDK's `javac` emitted; they move when the pinned JDK moves | **The most dangerous rule here.** This VM has had real wrong-bci bugs (`MEMORY.md`: `athrow_bci`). Never enable on an exception-attribution seed. No built-in profile turns it on |
| `sort-lines` | Whole-line ordering | `HashMap`/`HashSet` iteration order is unspecified, and CratonVM's capacity growth differs from HotSpot's | Every ordering bug vanishes: reversed lists, wrong stack direction, bottom-up traces. Prefer sorting *inside* the seed (`new TreeSet<>(…)`) and keeping the comparison strict |
| `ansi` (stderr only) | ANSI/VT escapes | Whether the capture pipe is detected as a tty is a property of the harness, not the program | A program emitting terminal control codes would be flattened |
| `vm-diagnostics` (stderr only) | Lines containing `[cratonvm]`, `[NativeBridge]`, `cratonvm_*` | The VM narrating its own startup/shutdown, which HotSpot has no counterpart for | A *program* printing one of those tokens loses the line; a VM warning that is itself the finding is erased. The `jdk-only` census counts the same events structurally, which is the cross-check |

**Ordering** is fixed by `normalize::ORDER` and is not commutative: stderr-only
rules first (`ansi` before `vm-diagnostics`, because the tracing lines are
colourised and a match on `cratonvm_` only works once the escapes are gone), then
line-ending hygiene, then the masking rules, then `sort-lines` last because
sorting must observe final line text.

**Every run prints its active rule set** on the second line
(`normalization: line-endings | stderr ungated`). If a rule is on that should not
be, it is at the top of the log rather than buried in a source file.

---

## 3. The checksum dimension

A program declares a checksum by printing, on its own line:

```text
##DIFFTEST-CHECKSUM## <name> <value>
```

The harness extracts these from **un-normalized** stdout
(`difftest/src/checksum.rs`) and compares them as their own dimension. Three
properties follow, and they are the whole point:

1. **A divergence names the quantity**, not a byte offset. `arith=111` vs
   `arith=222` is a bisection starting point; "line 37 differs" is not.
2. **No normalization rule can launder it.** If `mask_hashes` makes two `0x…`
   digests compare equal on stdout, the checksum dimension still fires — and,
   being the *only* dimension that fires, says exactly that. This is the
   harness's guard against its own over-normalization
   (`a_checksum_survives_an_over_normalizing_stdout_rule`).
3. **Agreeing checksums narrow a stdout diff.** If the checksums match while
   stdout diverges, the divergence is in formatting or in output the program did
   not intend as a result — a cheaper class of finding.

A program that declares nothing simply has no checksum dimension; the harness
reports nothing rather than reporting a vacuous match. `checksum::fnv1a64_hex`
is offered as a spelling a five-line Java helper can reproduce without touching
`java.security` (whose provider stack is itself under test).

---

## 4. The mode axis

CratonVM does not have one executor. It has **four semantic implementations**
plus two cross-cutting transitions, and a fix that lands in one and not the
others is a wrong-code risk:

| Path | Where it lives |
|---|---|
| Interpreter raw fast path (with superinstruction fusion) | `vm/src/runtime/interpreter.rs` ~9280 |
| Interpreter decoded fallback (quickened / full decode) | `vm/src/runtime/interpreter.rs` ~11574 |
| Single-pass "direct" x64 emitter (C1) | `jit/src/x64.rs` via `jit/src/lib.rs` |
| Optimizing IR pipeline (C2) | `jit/src/lib.rs` → `ir_optimize` → `ir_lower` |
| Back-edge OSR | `vm/src/runtime/env_cache.rs` ~246 / ~362 |
| Deoptimization (compiled → interpreted) | `jit/src/x64.rs` ~23589, `jit/src/lib.rs` ~1118 |

`--modes` drives them. Each mode is its own subprocess, and every
`CRATONVM_*` knob any mode can set is **cleared on every child** before the
active mode re-applies its own (`ALL_MODE_KNOBS` in `difftest/src/runner.rs`) —
without that, a stale `CRATONVM_JIT_FORCE_C2` exported in a developer's shell
would make every "direct emitter" row silently an IR row, and the cross-path
comparison would compare a path against itself and report a clean sheet.

### The six execution-path modes and the real flags they set

Every flag was verified against a live read site; the read site is quoted on the
`Mode` variant in `difftest/src/runner.rs` and asserted by
`execution_path_modes_set_the_flags_their_docs_name`.

| Mode | Flags | Read site |
|---|---|---|
| `nojit` *(pre-existing; this is the interpreter-**fast** row)* | `CRATONVM_DISABLE_JIT=1` | scalar flag, `docs/flag-tokens.md` §"The five scalars" |
| `interp-decoded` | launcher `--noverify` + `CRATONVM_DISABLE_JIT=1` | `vm/src/runtime/interpreter.rs:9280` — `let use_fast_path = !shared.config.skip_verification;`; flag parsed at `vm-cli/src/main.rs:1224` |
| `direct-emit` | `CRATONVM_NO_IR_BRANCHY=1`, `CRATONVM_JIT_IR_CALL=0`, `CRATONVM_JIT_THRESHOLD=1` | `jit/src/ir_optimize.rs:95` + `jit/src/lib.rs:9915`; `vm/src/runtime/env_cache.rs:1089` |
| `ir-jit` | `CRATONVM_JIT_FORCE_C2=1`, `CRATONVM_JIT_THRESHOLD=1` | `jit/src/lib.rs:8026` and `:9243` (`let optimize = optimize \|\| force_c2_enabled();`) |
| `osr-eager` | `CRATONVM_JIT_OSR=1`, `CRATONVM_TIER_OSR_BACKEDGE=1`, `CRATONVM_JIT_THRESHOLD=1` | `vm/src/runtime/env_cache.rs:246` (master enable, default ON) and `:362` (live per-frame back-edge trigger, clamped ≥ 1) |
| `no-osr` | `CRATONVM_JIT_OSR=0` | same master enable; the **control** for `osr-eager` |
| `forced-deopt` | `CRATONVM_DEOPT_EAGER=1`, `CRATONVM_DEOPT_REAL=1`, `CRATONVM_DEOPT_VERIFY=1`, `CRATONVM_JIT_THRESHOLD=1` | `jit/src/lib.rs:1182` / `:1135` / `:1166`; consumed at `jit/src/x64.rs:23589` to force a reason-2 deopt exit at the first speculative guard |

Note that the VM's flag surface was consolidated into ten grouped variables plus
five scalars (`docs/flag-tokens.md`); the legacy spellings above are exactly what
the grouped tokens expand to, and the harness sets them directly so that a
`CRATONVM_JIT=…` token list inherited from the environment cannot merge with the
harness's intent.

### Caveats, stated rather than papered over

* **`interp-decoded` also disables bytecode verification.** For a
  `javac`-produced corpus that is inert (the classes verify anyway), so the only
  observable change is which interpreter handlers run. It is **not** inert for
  the `mutate` tier: a mutated class that HotSpot would reject at verification
  will be executed instead. Do not read an `interp-decoded` divergence on a
  mutant as an interpreter bug without checking that first.
* **`direct-emit` is not a hard "IR off" switch, because the VM exposes none.**
  It suppresses IR routing for the shapes the VM lets us suppress (branchy
  call-free methods, IR call lowering); methods the IR builder can still fully
  build take the IR pipeline regardless. See §7.

---

## 5. Cross-path comparison: the finding that needs no reference

`difftest/src/crossmode.rs` compares every comparable pair of CratonVM runs
**against each other**, using the same eight dimensions.

> If `nojit` and `ir-jit` print different things for a deterministic program,
> one of them is wrong. No reference JDK is needed to know that, no environment
> difference explains it, and no normalization rule can be blamed for it — the
> two runs differ *only* in which of the VM's own executors ran the bytecode.

Two rules keep it honest:

1. **Same compatibility profile only.** A `--jdk-only` run compared against a
   compatible one would report the *policy* difference (a refused compatibility
   class, a missing native) as a semantic path split. Those pairs are skipped;
   the policy axis has its own ledger rows, keyed by `(class, jdk_profile)`.
2. **Report, never gate.** A `JitOnly` divergence already on the committed ledger
   *is* a path split by construction (`jit-on` disagrees with HotSpot while
   `nojit` agrees, so the two disagree with each other). Gating on path splits
   would fail the frozen baseline wholesale on findings that are already tracked.
   They appear in `run` output as `PATH` lines and in the gate report under
   "CratonVM execution paths disagreed with each other (reported, not gated)",
   exactly as the `jdk-only` violation census does.

Rule 2 applies to `run` and `gate`. **`path-gate` is the gated form**: it takes
`nojit` as the reference instead of HotSpot, needs no ledger, and fails on a
reproducible split that is not in `difftest/path-gate-known.json`. The bytecode
fuzzer `fuzz-jit` applies the same comparison to generated class files. See
[`jit-differential.md`](jit-differential.md).

---

## 6. The opcode / execution-path matrix

```bash
cratonvm-difftest matrix --corpus difftest/seeds --show-gaps
# → difftest/coverage-matrix.json
```

The opcode list is derived from the **corpus class files themselves**
(`difftest/src/matrix.rs`), not from a hand-maintained list: a hand-maintained
list is a list of what somebody remembered, and it rots silently. `.java` seeds
are compiled with the same `javac` the differential run uses, so the matrix
describes the exact bytes that run executes.

> A matrix is only as good as the corpus under it, and `difftest/seeds` is three
> programs. `cratonvm-difftest gen-opcodes` emits one program per opcode so the
> report has something to measure — see
> [`docs/testing/opcode-coverage.md`](opcode-coverage.md) for the generator, the
> five opcodes no Java source can produce, and the reconciliation step that keeps
> the generator's declarations honest against the compiled bytes.

### Reading a row

A row is keyed by `(opcode, operand form)`. The form is deliberately **bounded**
so the matrix stays a matrix — an operand that could take thousands of values
(a constant-pool index, a branch offset) is summarized by its shape, while an
operand that selects a different code path in every executor is kept:

| Form | Meaning |
|---|---|
| `implicit` | the operand is encoded in the opcode (`iload_0`, `iadd`) |
| `s8` / `s16` | `bipush` / `sipush` immediates |
| `cp8` / `cp16` | constant-pool index width (`ldc` vs `ldc_w`) |
| `local:u8`, `local:u8,s8` | local-slot operand, `iinc` |
| `branch:s16` / `branch:s32` | ordinary branch vs `goto_w`/`jsr_w` |
| `table` / `lookup` | `tableswitch` / `lookupswitch` |
| `atype:int`, `atype:double`, … | `newarray`'s element type — a different path in all four executors |
| `cp16,dims:N` | `multianewarray` dimension count |
| `wide:<op>` | a `wide`-prefixed instruction |

A **cell** is `(row × axis)` and is covered when **both** sides agree:

* **mode side** — a configured mode drives that axis (`matrix::axes_for`);
* **code side** — the opcode occurs somewhere the axis can reach: `osr` needs a
  method containing a **backward branch** (there is no back-edge to OSR from
  otherwise); `exception` needs a method with a non-empty exception table, or the
  opcode to *be* `athrow`.

Splitting the two is what makes a gap actionable: "`iaload` is not covered under
`osr`" has two possible fixes — configure the mode, or write a seed that puts an
`iaload` in a loop — and `axes_without_a_mode` in the JSON says which.

A cell is **not a boolean**. `CellState` (`difftest/src/matrix.rs`) is tri-state,
because a boolean cannot tell "nobody wrote a seed" from "nothing *can* write
one", and `jsr`/`ret` are permanently the second — JVMS §4.9.1 forbids them in
class files of version ≥ 50.0:

| State | Meaning |
|---|---|
| `covered` | mode side and code side both agree |
| `uncovered` | carries a `gap` (one of `no-mode-drives-axis`, `not-in-corpus-but-generated`, `no-generator`, `no-loop-site`, `no-handler-site`) and the `fix` text for it |
| `unreachable-by-construction` | carries the `reason`, and is never mixed in with ordinary gaps — "write a seed" is the wrong advice for these |

Corpus evidence outranks the declaration: a scanned `swap` beats an
`unreachable-by-construction` claim, and the contradiction is reported in the
JSON's `reconciliation` list rather than silently resolved either way. The
precedence order and the reconciliation output are documented in
[`opcode-coverage.md`](opcode-coverage.md) §5–§6.

### What the matrix deliberately does not claim

Running a program under `ir-jit` does **not** prove the IR pipeline compiled the
method containing a given opcode: admission is a conjunction of six independent
terms inside `jit/src/lib.rs`, any of which can decline silently. The matrix
reports **opportunity** — "the corpus contains this opcode and a mode driving
this path ran it" — not confirmed execution. Anything stronger needs the VM's own
per-method compilation report (`jit::metrics`), which the harness cannot see
across a process boundary. See §7.

---

## 7. Telling a harness false positive from a real VM divergence

Work down this list. The first question that answers "yes" stops the bisection.

1. **Did the determinism pre-flight run?** `gate` forces `--check-determinism`
   on; `run` does not. A program whose two HotSpot runs disagree is *rejected*,
   not judged. If you are looking at a `run` result without that flag, re-run
   with it before anything else.
2. **Does it reproduce?** `gate` forces `--reconfirm`, which re-runs the
   diverging mode **in the same mode** (never falling back to a laxer policy). A
   divergence that does not reproduce was a transient.
3. **Which dimension moved?** `checksum` alone ⇒ a real computed value differs
   and a normalization rule hid the stdout evidence. `stdout` alone with matching
   checksums ⇒ formatting. `exception-frames` alone ⇒ attribution, not
   semantics.
4. **Is it a path split?** A `PATH` line means two of CratonVM's own executors
   disagree. That is a VM defect with no reference involved — skip the rest of
   this list.
5. **Which normalization rules were active?** They are printed at the top of the
   run. If the divergence is inside something a rule *should* have neutralized,
   the seed is missing an opt-in. If a rule is on that should not be, you may be
   looking at a laundered result rather than a clean pass.
6. **Is the nondeterminism the program's own?** Identity hashes, `HashMap`
   iteration order, wall-clock, thread names, absolute paths. Each has a named
   rule in §2 — but prefer fixing the *seed* (`TreeSet`, a fixed instant, an
   explicit thread name) over enabling a rule, because every rule has a risk
   column.
7. **Is it the reference, not the VM?** The ledger header pins the JDK banner and
   host the baseline was captured against. A `frame-line-numbers` or
   `exception-message` divergence after a JDK upgrade is a reference change.
8. **Is it a policy finding wearing a semantic costume?** A row labelled
   `Class@jdk-only` is a *different finding* from the bare `Class` row, and a
   `known` compatible row never excuses a strict one. Check the
   `jdk-only violations (measured, not gated)` section before treating a strict
   divergence as wrong code.

---

## 8. Current coverage gaps

Honest list; each is a backlog item, not a claim of completeness.

**Mode axis**

* No mode forces the optimizing IR pipeline *off*. `direct-emit` suppresses IR
  routing only for the shapes the VM lets it suppress. **Required VM-side
  change:** a single admission-level opt-out (a `CRATONVM_JIT` token, e.g.
  `-ir`, checked next to `force_c2_enabled()` at `jit/src/lib.rs:9243`) would
  make "direct emitter only" exact instead of approximate. Out of scope for this
  change, which does not touch `jit/`.
* Path attribution is not confirmed per method. The harness cannot see
  `jit::metrics` across a process boundary. **Required VM-side change:** a
  `--dump-compilation-report <FILE>` launcher flag, alongside the three existing
  `jdk-only` census dumps in `vm-cli`, would let the harness assert *which* path
  actually compiled each method — turning the matrix from opportunity into
  confirmed coverage.
* `interp-decoded` is entangled with verification (§4). A dedicated
  "raw handlers off, verification on" switch would separate them.
* The pre-existing `moving-gc` mode sets `CRATONVM_NO_SELECTIVE_PROMOTE`, which
  is not the same knob as `CRATONVM_MOVING_YOUNG` (`docs/flag-tokens.md`,
  `CRATONVM_GC` group). Whether it selects the intended collector configuration
  is unverified here; the historical five modes are frozen by the committed gate
  baseline and were deliberately not touched.

**Corpus**

* The committed `seeds/` corpus is still three programs, and none of the three
  declares a checksum or is written to sit in a loop *and* under a handler. Run
  `cratonvm-difftest matrix --corpus difftest/seeds --show-gaps` for the current
  list; the great majority of the 202 named opcodes are unexercised there, and
  `unexercised_opcodes` in the JSON names every one of them.
* The **generated** corpus closes both of those on the corpus side, and only on
  the corpus side. `cratonvm-difftest gen-opcodes` emits 197 programs, every one
  of which prints `##DIFFTEST-CHECKSUM## op<nnn>-<mnemonic> <hex>` and wraps its
  focus code — and every generated helper method's body — in a loop inside a
  `try` (`opcorpus.rs`, held by `programs_wrap_their_focus_in_a_loop_and_a_handler`
  and `the_helper_builder_always_emits_a_loop_and_a_handler`). So the `checksum`
  dimension and the `osr` × `exception` cells now have programs that exercise
  them.
* **What is still open is running them.** The CI step added to `difftest-gate`
  *compiles* the generated corpus and reports the matrix; it does not diff it
  against HotSpot. A differential run over the generated corpus is 197 programs ×
  7 modes × two VMs, and is not wired anywhere. Until it is, the checksum
  dimension is *exercisable* rather than exercised, and new hand-written seeds
  should still declare a checksum for any quantity they compute.

**Comparison**

* Line-ending hygiene is applied at **capture** time
  (`runner::run_subprocess`) as well as in the oracle, so a trailing-whitespace
  divergence is unobservable end to end, not merely un-normalized.
* `stderr` remains ungated for every historical mode. Only the four profile
  modes judge it, and only after `vm-diagnostics` filtering.
* `gated_eq` (the ledger drift check) compares stdout, exit status and the
  exception struct, but not the checksum dimension. A drift confined to a
  declared checksum inside otherwise-identical stdout is impossible today (the
  declaration is on stdout), but would become possible if a future rule masked
  the declaration line.

---

## 9. Running it

```bash
# The execution-path contract: same program, every executor, cross-compared.
cratonvm-difftest run --corpus difftest/seeds \
    --modes nojit,interp-decoded,direct-emit,ir-jit,osr-eager,no-osr,forced-deopt \
    --check-determinism --reconfirm

# The CI gate (determinism + reconfirmation forced on).
cratonvm-difftest gate --corpus difftest/seeds

# Coverage matrix for the same mode list.
cratonvm-difftest matrix --corpus difftest/seeds --show-gaps
```

Gate exit codes are unchanged: `0` clean, `1` new-or-drifted divergence, `2` a
`fixed` divergence re-opened, `3` bootstrap (no JDK / no `cratonvm` binary /
empty corpus). Path splits and `jdk-only` violations are reported and never move
the exit code.
