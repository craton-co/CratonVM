# JIT differential testing: the path gate and the bytecode fuzzer

The HotSpot gate (`cratonvm-difftest gate`, [`differential.md`](differential.md))
answers "does CratonVM behave like a real JDK?". It does not answer "do
CratonVM's own JIT tiers compute what its interpreter computes?". Before this
lane, CI ran `gate` over six seed programs in `jit-on`, `nojit` and
`interp-decoded` only. No IR, OSR or deopt mode was gated, the IR verifier lanes
were never switched on in any workflow, and every fuzz target under `fuzz/` was
a parser fuzzer.

This document covers the two tools that close that gap. Neither needs a
reference JDK, because the interpreter (`nojit`) is the oracle.

| Tool | What it runs | Where it runs |
|---|---|---|
| `path-gate` | a corpus (`difftest/seeds`, `difftest/regression`, the generated opcode corpus) across every execution-path mode | PR CI (`difftest-jit-paths`), nightly |
| `fuzz-jit` | generated class files, written directly as bytecode | nightly (`jit-differential-nightly.yml`), explore (`jit-differential-explore.yml`); `--dry-run` in PR CI |

Section [6](#6-two-lanes-a-fixed-gate-and-a-rotating-explorer) describes how the
two `fuzz-jit` lanes divide the work, and why only one of them is allowed to
change its seeds.

## 1. Why the interpreter can be the oracle

For a deterministic program, two of the VM's executors printing different
things means one of them is wrong ([`crossmode.rs`](../../difftest/src/crossmode.rs)).
No normalization rule applies (stdout is compared byte-exact), no environment
difference explains it, and no reference JDK has to be installed, trusted or
pinned. The interpreter is the natural reference: it is the simplest executor
and the one every other path deoptimizes back into.

The weakness is that a bug shared by the interpreter and a JIT tier is
invisible. That is what the HotSpot gate is for, and `fuzz-jit --hotspot`
checks the interpreter against `java` on generated programs.

## 2. `path-gate`

```text
cratonvm-difftest path-gate [--corpus DIR] [--modes LIST] [--reference MODE]
                            [--known FILE] [--fail-on-ir-verify-reject]
```

* **Default modes:** `nojit,interp-decoded,direct-emit,ir-jit,osr-eager,no-osr,forced-deopt,moving-gc`,
  which is `Mode::execution_paths()` plus `moving-gc`. A unit test pins this.
* **Reference:** `nojit`. It runs **twice**. A program whose two reference runs
  differ is reported as nondeterministic and is not judged.
* **Judgement:** every other mode runs once and is compared with the reference
  on exit code, uncaught exception (type, message, frames), stdout and declared
  checksums. stderr is not gated.
* **Re-run:** a split is re-run once. A split that does not reproduce is
  reported as **flaky** and does not affect the exit code. A flaky split is
  still a finding (timing-dependent wrong code is a bug), but gating on it
  would make the job flap.
* **Known splits:** `difftest/path-gate-known.json` lists tracked
  `(program, mode)` splits. A listed split is allowed. A listed split that now
  agrees is reported as resolved so the entry can be removed. The file is
  committed empty, and each entry should name its `docs/known-issues/` record in
  `note`.
* **IR verifier:** a rejection is normally silent (the method falls back to the
  single-pass backend). With `CRATONVM_DBG_IR_COMPILES=1` exported,
  `jit/src/lib.rs::ir_verify_reject` prints `[ir] verifier rejected …`. The gate
  always reports that line. With `--fail-on-ir-verify-reject` it also fails the
  job.

Exit codes: `0` clean, `1` a new reproducible split (or a gated verifier
rejection), `3` bootstrap (no `cratonvm` binary, no `javac` for `.java` seeds,
empty corpus).

### Why not widen `gate`?

`gate` keys ledger rows by `(class, jdk_profile)`, not by mode. A new mode would
silently share the rows captured under `jit-on`/`nojit`, and those rows' drift
check compares the recorded CratonVM observation. `gate` also reports path splits
but deliberately never gates on them. Widening it would therefore mean
regenerating `difftest/ledger.json` against a local HotSpot for every mode added,
and it still would not gate a split. `path-gate` avoids both problems, and the
existing `gate` step stays byte-identical.

**No ledger regeneration is required** for either new tool. `difftest/ledger.json`
is untouched.

## 3. `fuzz-jit`

```text
cratonvm-difftest fuzz-jit --seed N --count K [--modes LIST] [--reference MODE]
                           [--extra-env KEY=VALUE]... [--class-version 52|49]
                           [--rounds R] [--out DIR] [--no-minimize] [--hotspot]
                           [--fail-on-ir-verify-reject] [--max-failures F]
                           [--dry-run [--dump DIR]]
```

### Program shape

Each seed produces one class `JitFuzz_<seed>` with two methods:

* `static long t(int a, long b, float f, double d)`: three to six **snippets**.
  Each snippet starts and ends with an empty operand stack and folds one `long`
  result into an FNV accumulator. That makes any subset of snippets a valid
  method, which is what makes minimization a list operation.
* `main`: prints `t(...)` once per input (six to ten inputs from edge-value
  pools), then calls `t` over every input for `--rounds` iterations (default
  256). The loop makes the JIT tiers compile `t` and gives OSR a hot loop to
  enter. `main` finally prints `##DIFFTEST-CHECKSUM## jitfuzz <acc>`.
  Inputs marked `vary` XOR `a` with `round & 3`, so not every call site can be
  constant-folded.

Float and double results go through `Float.floatToIntBits` /
`Double.doubleToLongBits`. Those canonicalize NaN, so a JIT that legally
produces a different NaN payload is not reported, while `-0.0` versus `0.0` is
still distinguished.

### Categories

The first snippet of seed `s` is `Category::ALL[s % 16]`, so any 16 consecutive
seeds cover every category. The rest are drawn from the PRNG.

| Category | What it emits |
|---|---|
| `div-rem-overflow` | `MIN_VALUE / -1`, `% -1` for int and long, with constant and computed (`a \| 1`) divisors |
| `div-by-zero-caught` | `idiv`/`irem`/`ldiv`/`lrem` by an input that may be zero, inside an `ArithmeticException` handler |
| `shift-counts` | `ishl`/`ishr`/`iushr`/`lshl`/`lshr`/`lushr` by -1, 0, 31, 32, 33, 63, 64, and by an input |
| `fp-compare` | `fcmpl`/`fcmpg`/`dcmpl`/`dcmpg` against NaN, -0.0, 0.0, ±Inf, as a value and fused into `if<cond>` |
| `fp-to-int` | `f2i`/`f2l`/`d2i`/`d2l` (and `d2f`→`f2i`) of NaN, ±Inf, out-of-range and boundary values |
| `fp-rem` | `frem`/`drem` in both operand orders against 0, -0, ±Inf, NaN, subnormals |
| `int-narrowing` | `i2b`/`i2c`/`i2s`, chained and after overflowing adds |
| `switches` | `tableswitch` with ranges touching `MIN_VALUE`/`MAX_VALUE`; `lookupswitch` over edge keys |
| `loop-bounds` | loops entered with `i > n`, zero-trip counted loops, `i <= n` with `n == MAX_VALUE`, wrapping down-counters, do-while (guard-bounded) |
| `array-long-sum` | `int[]` of large values summed into a `long` (or a wrapping `int`) accumulator |
| `double-sum-order` | `double[]` `{1e16, 1, -1e16, 1}` permutations summed forward or backward |
| `array-alias` | `ib = ia` (always, or only when `a < 0`), then `ia[k] = ib[k-1] + c` |
| `ternary` | `(c && a < b) ? a : b`, `(a < 0 \|\| a > b) ? …`, and `z * (c ? d : 2.0)` in a loop |
| `handler-null-join` | `o = ia; try { … } catch { o = null; }`, then `o == null ? … : ((int[]) o)[0]` |
| `stack-shuffles` | every JVMS form of `dup_x2`, `dup2_x1`, `dup2_x2`, plus `swap`, `pop2` (both forms), `dup_x1`, `dup2` |
| `wide-locals` | `wide` load/store/`iinc` on small slots, and a loop whose induction variable is slot 300 |

### Verifiability

Classes are written by [`classgen.rs`](../../difftest/src/classgen.rs), a
*typed* assembler. Every emitter carries its JVMS type effect, and the
assembler checks the following as it goes:

* every pop has the expected type;
* every branch state is assignable to its target's declared frame;
* fall-through into a label is checked the same way;
* code after `goto`/`return`/`athrow`/a switch is a bound label;
* every state inside a `try` range satisfies the handler's frame.

It also emits a `full_frame` `StackMapTable` entry at every label (major 52).
`check_class_shape` then re-parses the written bytes and checks instruction
boundaries, branch and handler targets, and frame placement. The unit tests run
both checks over 128 seeds and over each category in isolation.

`--class-version 49` writes the same code without `StackMapTable`. The
verifier then takes the type-inference path. This is the fallback if a frame
is ever rejected, and the nightly also runs it as its own axis.

### Axes

`--extra-env KEY=VALUE` is applied to every **non-reference** mode, after the
runner clears and re-applies that mode's own knobs. This is how a mode is
combined with a knob it does not set:

| Axis | Flags |
|---|---|
| default | `--modes nojit,direct-emit,ir-jit,osr-eager,forced-deopt,moving-gc` |
| GC stress | same modes, `--extra-env CRATONVM_DBG_GC_STRESS=65536` |
| eager deopt | `--modes nojit,direct-emit,ir-jit,osr-eager,moving-gc --extra-env CRATONVM_DEOPT_EAGER=1 --extra-env CRATONVM_DEOPT_REAL=1 --extra-env CRATONVM_DEOPT_VERIFY=1` |
| no stack maps | `--class-version 49 --modes nojit,interp-decoded,ir-jit,osr-eager,forced-deopt` |

`forced-deopt` already sets `CRATONVM_DEOPT_EAGER`, `CRATONVM_DEOPT_REAL`,
`CRATONVM_DEOPT_VERIFY` and `CRATONVM_JIT_THRESHOLD=1`, so the eager-deopt
axis is not repeated on it. No mode sets `CRATONVM_DBG_GC_STRESS`.

### Output

A failing program is written to `<out>/JitFuzz_<seed>/`:

* `JitFuzz_<seed>.class`: the minimized program, runnable with
  `-cp <out>/JitFuzz_<seed>`. The shrinker removes snippets, then inputs, then
  halves `rounds`, while the split still reproduces.
* `original/JitFuzz_<seed>.class`: the program as generated.
* `report.txt`: each split's differing dimensions, a ready-to-paste repro command
  (mode env, extra env, launcher flags), the regenerate command, and each
  side's stdout plus the tail of stderr.
* `program.txt`: categories, inputs (with raw float bits) and a disassembly of
  `t`, for both versions.

Failure kinds: `SPLIT` (a mode disagrees with the reference), `INVALID` (the
reference itself did not exit 0 with a checksum; a `VerifyError` there is a
generator bug or a CratonVM verifier bug, so rerun with `--hotspot`), `BUILD`
(the assembler rejected the program; always a generator bug), `HOTSPOT` (only
with `--hotspot`: the interpreter disagrees with `java`).

Exit codes: `0` nothing failed, `1` at least one failing program, `3` bootstrap.

## 4. Running it locally

```bash
# Build (release is far faster to run; see the release-build-cost note).
cargo build --release -p cratonvm-cli -p cratonvm-difftest
export CRATONVM_BIN=target/release/cratonvm
DT=target/release/cratonvm-difftest

# The IR verifier lanes, and the switch that makes a rejection visible.
export CRATONVM_JIT_VERIFY_IR=1 CRATONVM_JIT_VERIFY_TYPES=1 \
       CRATONVM_JIT_VERIFY_FRAME_STATES=1 CRATONVM_JIT_VERIFY_MEMORY_CHAIN=1 \
       CRATONVM_JIT_VERIFY_ARENA_ORDER=1 CRATONVM_DBG_IR_COMPILES=1

# The PR lane.
$DT path-gate --corpus difftest/seeds --fail-on-ir-verify-reject

# The generator's own checks, no VM (also what PR CI runs).
$DT fuzz-jit --dry-run --seed 1 --count 256

# The nightly axes.
$DT fuzz-jit --seed 1 --count 400 --fail-on-ir-verify-reject
$DT fuzz-jit --seed 1 --count 400 --extra-env CRATONVM_DBG_GC_STRESS=65536
$DT fuzz-jit --seed 1 --count 400 --modes nojit,direct-emit,ir-jit,osr-eager,moving-gc \
    --extra-env CRATONVM_DEOPT_EAGER=1 --extra-env CRATONVM_DEOPT_REAL=1 --extra-env CRATONVM_DEOPT_VERIFY=1
$DT fuzz-jit --seed 1 --count 400 --class-version 49 --modes nojit,interp-decoded,ir-jit,osr-eager,forced-deopt

# The opcode corpus through the path gate.
$DT gen-opcodes --out target/difftest-opcodes
$DT path-gate --corpus target/difftest-opcodes

# The promoted repros the nightly now also gates (§6).
$DT path-gate --corpus difftest/regression --known difftest/path-gate-known.json

# One explore night. Same four axis commands as above with a different --seed;
# read the number off the run's `explore window` notice, or compute it (§6).
$DT fuzz-jit --seed $(( 1000000 + $(( $(date -u -d 2026-09-16 +%s) / 86400 )) * 10000 )) \
    --count 400 --fail-on-ir-verify-reject --max-failures 5
```

PowerShell: `$env:CRATONVM_BIN = "target\release\cratonvm.exe"` and
`target\release\cratonvm-difftest.exe …`. Note that FP-register defects only
reproduce on Win64, so a Linux-green seed is worth rerunning on Windows.

### Reproducing one failure

```bash
# 1. Regenerate exactly one seed, against just the failing mode, unminimized.
$DT fuzz-jit --seed 1234 --count 1 --modes nojit,ir-jit --no-minimize --out /tmp/jf

# 2. Run the written class directly (copy the env from report.txt's repro line).
CRATONVM_JIT_FORCE_C2=1 CRATONVM_JIT_THRESHOLD=1 \
    target/release/cratonvm -cp /tmp/jf/JitFuzz_1234 JitFuzz_1234
CRATONVM_DISABLE_JIT=1 target/release/cratonvm -cp /tmp/jf/JitFuzz_1234 JitFuzz_1234

# 3. Check the class against a real JVM and its verifier.
java -Xverify:all -cp /tmp/jf/JitFuzz_1234 JitFuzz_1234
javap -c -v -cp /tmp/jf/JitFuzz_1234 JitFuzz_1234

# 4. Rule out the interpreter: compare it against HotSpot on the same seed.
$DT fuzz-jit --seed 1234 --count 1 --modes nojit --hotspot
```

Generation is seed-deterministic (no clock, no hash-order iteration). The same
`--seed`, `--rounds` and `--class-version` always produce the same bytes, and
`generation_is_byte_identical_per_seed` enforces it.

## 5. Mode notes and missing VM hooks

* **Compile at first call.** `CRATONVM_JIT_THRESHOLD` is the invocation count at
  which a method becomes *eligible* for compilation, clamped to at least 1
  (`vm/src/runtime/env_cache.rs::jit_invocation_threshold`).
  `low-jit-threshold`, `direct-emit`, `ir-jit`, `osr-eager` and `forced-deopt`
  already set it to 1, so no new mode was added. Two caveats remain: nothing in
  difftest can prove that the compile is **synchronous** (the call that crosses
  the threshold may still be interpreted), and methods the JIT declines stay
  interpreted silently. *Missing hook:* a "compile synchronously before the
  first compiled-eligible call, and fail loudly if the compile bails" switch.
  With it, a mode could guarantee that `t` ran compiled.
* **Deopt at every safepoint.** Not available. `CRATONVM_DEOPT_EAGER` (with
  `CRATONVM_DEOPT_REAL`) forces one reason-2 deopt exit at the first
  speculative guard / loop header in single-pass code (`jit/src/lib.rs::deopt_eager_enabled`,
  consumed in `jit/src/x64/driver.rs`). `CRATONVM_DEOPT_EAGER_BCI=<n>` moves
  that single trigger to bytecode index `n`, and `CRATONVM_OSR_EXIT_AFTER=N`
  bails at a loop header on the N-th reach. None of these deoptimizes at *every*
  safepoint, and `CRATONVM_DEOPT_EAGER_BCI` is meaningless for generated
  programs whose bcis vary by seed. *Missing hook:* a
  `CRATONVM_DEOPT_STRESS=every-safepoint` knob (and an IR-tier equivalent).
* **IR verifier visibility.** A rejection is recorded in bailout counters and
  printed only under `CRATONVM_DBG_IR_COMPILES`/`CRATONVM_DBG_JITC`. *Missing
  hook:* a `CRATONVM_JIT_VERIFY_FATAL=1` that aborts on a rejection, so the
  gate would not depend on scraping a debug line.
* **`direct-emit` is not a hard "IR off" switch** ([`differential.md`](differential.md) §4).
  A method the IR builder fully builds still takes the IR pipeline.

## 6. Two lanes: a fixed gate and a rotating explorer

A fixed seed range is the right input for a gate and the wrong input for a
search, and `fuzz-jit` is asked to be both. The two demands do not fit in one
workflow, so they are two.

| | gate lane | explore lane |
|---|---|---|
| file | `.github/workflows/jit-differential-nightly.yml` | `.github/workflows/jit-differential-explore.yml` |
| seeds | fixed: `--seed 1 --count 400` | rotating: a fresh 400-seed window every UTC day |
| schedule | `17 3 * * *` | `47 4 * * *` |
| axes | default, GC stress, eager deopt, `--class-version 49` | the same four, unchanged |
| blocks a merge | no, but a red night is a real failure someone owns | no, and a red night is expected output |
| red means | a regression against a known-green input | a candidate finding |
| reports by | failing the job | opening or commenting on a `jit-explore` issue |

### Why only one of them may rotate

The nightly's seed range is fixed so that a red night is reproducible on the
next night and on a laptop with the same command. Rotate it and the lane can no
longer tell "the compiler regressed" from "today's dice rolled differently",
and a re-run after the fix proves nothing because it runs different programs.
The cost of that discipline is that the lane's coverage is a *constant*: night
400 executes exactly the 400 programs night 1 executed. Those 400 stop paying
for themselves the day they first go green.

The explore lane pays a different price for a different property. It gates
nothing, so it is free to run seeds nobody has run before, which is the entire
product. In exchange it owes reproducibility of any *given* night, and that is
what makes rotation acceptable:

* the window is a pure function of the UTC date — no `$RANDOM`, no run id, no
  sub-day clock;
* the resolved window and a copy-pasteable local command are printed as a
  `::notice::` and into the job summary **before** the fuzzer runs and again
  after it, on a pass and on a fail;
* generation is seed-deterministic, pinned by
  `generation_is_byte_identical_per_seed`.

If those printing steps ever go, the rotation has to go with them.

### The window

```text
SEED_START = 1000000 + (whole UTC days since 1970-01-01) * 10000
```

Each night gets its own disjoint block of 10,000 seeds, so no two nights ever
revisit each other, and the `1000000` base keeps every block a million seeds
clear of the gate lane's `--seed 1 --count 400`. The workflow asserts
`count <= 10000` rather than trusting it, because a `workflow_dispatch` with a
large `count` is exactly how the disjointness would be violated quietly. The
stated limit: dispatching the *nightly* with a `count` near 10^6 would walk
into the explore blocks. Nothing enforces that, and the nightly's committed
default is 400.

Recompute a night by hand:

```bash
EPOCH_DAY=$(( $(date -u -d 2026-09-16 +%s) / 86400 ))
echo $(( 1000000 + EPOCH_DAY * 10000 ))
```

`workflow_dispatch` takes the same `seed_start` / `count` inputs as the
nightly; supplying `seed_start` pins the window and skips the date arithmetic.
A dispatched run deliberately files no issue — the person who dispatched it is
already reading the log.

### Minimization, and why `min` is not what runs

`fuzz-jit` shrinks each failing program itself, in process, before writing it:
snippets first, then inputs, then `rounds`, while the split still reproduces
(§3, *Output*). The `JitFuzz_<seed>.class` the explore lane uploads **is** the
minimized reproducer, which is why `--no-minimize` is not passed.

The `min` subcommand is not used and cannot be. It is a *source* shrinker and
rejects anything that is not `.java`; a `fuzz-jit` program has no Java source at
any point, because `classgen.rs` emits it straight to class bytes. Running
`min` on this lane's output exits 3 without shrinking anything. What is missing
from the CLI is a shrinker that takes a written `.class` — until then, a repro
can only be re-shrunk by regenerating its seed.

### Promotion: a confirmed explorer finding becomes gate corpus

A finding that stays in an issue is a finding the lane will make again next
year. The rule is that a **confirmed** explorer finding is promoted into the
gate corpus:

1. Reproduce it locally from the command in the issue. A split that will not
   reproduce is a nondeterminism finding, not a JIT finding; file it as that.
2. Fix the bug. `difftest/regression/` is defined by
   [the design](../feature-designs/differential-fuzzer.md) §3.1 as "every
   confirmed-then-*fixed* divergence, minimized, committed so a future
   regression re-trips it", so the promotion normally rides along with the fix
   in one PR and is green the day it lands.
3. Commit the minimized `JitFuzz_<seed>.class` (not `original/`) as a **flat
   file** in `difftest/regression/`, next to the existing `ExceptionId.java`.
   `harness::discover_programs` accepts `.class` as well as `.java`, so a
   bytecode-only repro is a first-class member — but it is a single `read_dir`
   with **no recursion**, so a repro nested in the per-repro subdirectory the
   design's §3.3 sketches is skipped silently. The nightly's *Path gate over
   the promoted regression repros* step runs that directory every night from
   then on.
4. Only if the repro has to land *before* its fix: add the `(program, mode)`
   split to `difftest/path-gate-known.json` with the issue named in `note`, or
   that nightly step is red from the day you commit it. `path-gate` reports a
   listed split that now agrees as **resolved** — deleting the entry at that
   point is what turns the repro into a gate. This is the exception, not the
   route.

Three limits, stated rather than papered over:

* **This is nightly coverage, not PR coverage.** `ci.yml`'s
  `difftest-jit-paths` reads `difftest/seeds`. Adding `difftest/regression`
  there is a `ci.yml` change that also has to reckon with `difftest-gate`
  reading the same directory and judging it against HotSpot, which may want a
  `difftest/ledger.json` row. That coupling is why promotion is a human edit
  and why the explore lane opens an issue rather than a PR.
* The regression step runs **without** `--fail-on-ir-verify-reject`.
  `--known` can excuse a split; it has no entry shape for a verifier rejection,
  so a promoted repro that also trips the IR verifier would pin the nightly red
  with no way to acknowledge it. The rejection is still printed and reported.
* Until that step was added, **nothing in CI read `difftest/regression/` at
  all**. `min` wrote there, `difftest/src/lib.rs` called it the home of the
  minimal repro, the design specified it — and `ExceptionId.java` sat in it
  unexecuted. A directory every document calls the regression corpus and no job
  ever runs is a corpus in name only.

### Deduplication: one open issue per axis

The explore lane keys its issue on the axis, not on the seed. Keying on the
seed is the obvious choice and is wrong *because* the window rotates: every
night's seeds are new, so one unfixed defect would file a fresh issue every
night. The accepted cost is that two genuinely different bugs in the same axis
arrive as two comments on one thread. Splitting a conflated thread is one edit;
de-duplicating thirty issues about one bug is thirty.

## 7. What this does not claim

* A clean night is not proof of correctness. The categories are deliberately
  narrow, and `t` is a leaf method with no calls other than the two
  `*ToBits` intrinsic candidates, no objects beyond primitive arrays, and no
  virtual dispatch. Inlining, class hierarchy analysis and escape analysis of
  real objects are not exercised.
* Shared interpreter/JIT bugs are invisible without `--hotspot`.
* Flaky splits are reported but not gated. Read them anyway.
