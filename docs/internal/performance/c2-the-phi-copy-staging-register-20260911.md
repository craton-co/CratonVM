# A phi copy staged in RAX, and the pairing census that said where to look

**2026-09-11.** Every phi edge copy in the optimizing tier was staged through
RAX and then published into the phi's own register — `mov rax, r15` /
`mov r12, rax` — so a loop paid **one instruction per loop-carried value per
iteration** to move a value that was already in a register.

Staging in the destination's register instead is worth **1.05x to 1.12x** on
`probes/FieldLoop.java` `sum` across three invocations that agree on the sign,
and takes the tiering inversion on that shape from **1.208x to ~1.11x** on this
host. `CRATONVM_JIT_IR_PHI_COPY_DIRECT=0` is the kill switch.

Sibling of
[`c2-one-carry-slot-is-the-frame-traffic-ceiling-FIXED-20260910.md`](c2-one-carry-slot-is-the-frame-traffic-ceiling-FIXED-20260910.md)
and
[`c2-the-gp-register-file-is-not-the-binding-constraint-20260910.md`](c2-the-gp-register-file-is-not-the-binding-constraint-20260910.md),
and it starts where the first of those ended: with the census it asked for.

## 1. The census the carry page asked for

§10 of the carry page closed on a number it could not break down: **82% of the
deferred carry's candidate windows are declined for `operand_position`** — the
triple `[input1, input0, cons]` that both carry slots need was never formed.
`ir_schedule::pair_single_use_operands` is the pass whose job is forming it, it
declines for five enumerated reasons, and:

> **Which of those four dominates is not yet counted** — the pairing pass has no
> census, and that is the next thing to build, not the next thing to fix. It is
> one counter per `continue` in a 60-line function.

Built: `ir_schedule::PairCensus`, one counter per `continue`, under an
accounting identity (`paired + multi_use + no_node + producer_arm + other_block
+ after_consumer + already_adjacent + deopt_between == candidates`) that is
`debug_assert`ed at the publish site, so a reason added to the pass and not to
the census fails `cargo test` instead of reading low. That is the failure the
GP-register page's §10.1 records — *a census that computes its own answer
instead of reading the one the code used will drift, and it will drift towards
zero.*

It prints per method as `[ir-ls] operand pairing:` under
`CRATONVM_DBG_IR_LINEAR_SCAN=1`, and process-wide as
`[c2-supersede] ir operand pairing:` under `CRATONVM_DBG=jitc`, beside the
deferred-carry line it explains. **The denominator is `(consumer, operand)`
pairs where the operand is a real, distinct node** — every pair the pass could
conceivably act on and nothing else, which is the lesson the sibling census
learned when its first cut reported every three consecutive scheduled nodes and
said nothing.

### What it says on the loop the inversion is about

`FieldLoop.sum` — `for (i = 0; i < n; i++) a += this.fx;` — under
`CRATONVM_JIT=force-c2`:

```text
[ir-ls] operand pairing: candidates=16 paired=0 | declined:
        multi_use=12 producer_arm=4 other_block=0 after_consumer=0
        already_adjacent=0 deopt_between=0 no_node=0
```

**Position is not the constraint here.** The four buckets that together mean
"the operand is not where the carry needs it" — `other_block`,
`after_consumer`, `already_adjacent`, `deopt_between` — are **zero**. Every
decline is the operand being read more than once (75%) or its defining arm not
being one `op_home_is_one_store_rax` certifies (25%).

Those two are not gaps the pairing pass can close, and the reason is the shape
of a counted loop rather than anything about this pass. A loop's carried values
ARE its phis, a phi is read by its own back edge as well as by the body, and a
multi-use value can never be carried — sinking it past a reader would be a
semantic change, not a scheduling one. **Carrying is a mechanism for
single-use intermediates, and a counted loop's cost is in values that are not
single-use.** The 82% `operand_position` figure the carry page reports is a
statement about the emitter's candidate windows across a whole probe set; it
does not mean the pairing pass is what a LOOP is waiting for.

### And what it says across the probe set

Every probe in `probes/` that compiles alone, run under
`CRATONVM_JIT=force-c2 CRATONVM_DBG=jitc`, one process each — 201 launched, 188
produced a census line:

| cause | windows | share |
|---|---:|---:|
| **candidates** | **34,289** | 100% |
| paired | 573 | 1.7% |
| **declined — producer is multi-use** | **26,865** | **78.3%** |
| **declined — producer's arm not certified** | **5,560** | **16.2%** |
| declined — already adjacent | 956 | 2.8% |
| declined — producer in another block | 238 | 0.7% |
| declined — a deopt can arrive in between | 97 | 0.3% |
| declined — producer after its consumer | 0 | 0.0% |
| declined — no node | 0 | 0.0% |

**This retires the next step the carry page proposed.** That page reads its own
82% `operand_position` as "the triple the carry needs was never formed", and
names `pair_single_use_operands` as what would form it. The four buckets that
mean *position* — already adjacent, another block, a deopt in between, after
the consumer — are together **3.8%** of every window the pass sees. The pass is
not failing to place eligible operands; **94.5% of them were never eligible**,
and no amount of scheduling changes that.

So: *do not start work on the pairing pass from the 82% figure.* The two
numbers describe different denominators — the carry's is windows where a
consumer already takes its first operand in RAX, the pass's is every
`(consumer, operand)` pair — and only this one says what the pass could act on.

**`producer_arm` at 16.2% is the one actionable row**, and it is
`ir_lower::op_home_is_one_store_rax` declining. Two of its exclusions account
for most ordinary code and they want opposite conclusions: `Op::Const` is
excluded and should stay so — `ir-alu-imm` folds a constant operand into the
instruction, which is better than carrying it — while **`Op::Load`, the
`getfield`, is excluded for a reason that is about its ARM and not about the
value**. Its three paths (inline compact, inline legacy, checked helper) are
mutually exclusive and each ends in one `store_rax` with RAX holding the
result, but the mechanical test counts `self.store_rax(slot);` textually and
sees three. A per-op breakdown of this row is the census to take next, and it
is one more counter.

## 2. The inversion, re-measured on this host

Not quoted forward. `probes/FieldLoop.java` `sum`, `probe.reps=25000
probe.n=20000`, one release binary at `1d9c00029` + the census, arms
interleaved with a same-config control:

| instrument | A (single-pass) | C (control) | B (optimizing) | floor | effect |
|---|---:|---:|---:|---:|---:|
| wall clock, probe's own `ms=` | 220.0 ms | 239.5 ms | 276.0 ms | 8.5% | **+20.1%** |
| user CPU | 0.797 s | 0.781 s | 0.953 s | 2.0% | **+20.8%** |

**1.208x**, checksums identical (`acc=480150000`) in every run. The two
instruments agree to 0.7 points while their floors differ four-fold, which is
the best evidence either of them is reading the lever rather than the machine.

This is a 32-core Windows box under other tenants, not the host the 2026-09-10
page measured 1.594x on, and not the Linux box that read 1.753x. **Compare the
ratios within a host, never across them.**

## 3. The register file is not it — a third witness, and a stronger one

`c2-the-gp-register-file-is-not-the-binding-constraint-20260910.md` concluded
that widening the GP file does not pay, from six invocations across two shapes.
It reproduces here, and the disassembly adds what those runs could not say:
**exactly what the two extra registers bought.**

With `CRATONVM_JIT_IR_GP_WIDE=0`, the loop's induction variable makes its round
trip through the frame:

```asm
259: lea  eax,[rbx+1]         ; i + 1
25c: mov  [rbp-98h],rax       ; home store
...
369: mov  rax,[rbp-98h]       ; reloaded across a store-forwarding stall
370: mov  rbx,rax
```

With `=1`, RSI/RDI join the file on Win64 and that recurrence leaves memory
entirely — `260: lea r15d,[rbx+1]`, no home store, no reload, the phi copy
reading a register:

| | `GP_WIDE=0` | `GP_WIDE=1` |
|---|---|---|
| `i + 1` | frame slot | **`r15`** |
| home store + reload on the back edge | yes | **gone** |

So the lever removes the exact store-to-load-forwarding pair that
`docs/JIT_OPTIMIZATION.md` traced this tier's residual to. And:

| arm | median user CPU |
|---|---:|
| A — `GP_WIDE=0` | 0.961 s |
| C — control | 0.969 s |
| B — `GP_WIDE=1` | 0.969 s |

Floor **0.8%**, effect **+0.4%: UNMEASURABLE**, checksums identical.

**That is worth more than another null.** The earlier page had to argue from
census counters that more registers do not pay. Here the recurrence provably
leaves memory and the loop does not get faster — so the store-forwarding chain
is **not** this loop's critical path, whatever its latency is in isolation.
Anyone reaching for "the loop-carried value round-trips through the frame" as
the explanation has to answer this measurement first.

## 4. What the disassembly says instead: a per-iteration instruction budget

Both tiers' `FieldLoop.sum`, hot path only (the safepoint-poll slow path and
the `getfield` helper path excluded), counted per ITERATION because the
single-pass tier unrolls 4x and the optimizing tier does not:

| | single-pass | optimizing |
|---|---:|---:|
| instructions per iteration | ~19 | **26** |
| taken branches per iteration | 2 | **3** |

Where the optimizing tier's extra seven go:

| cost | instructions | who else pays it |
|---|---:|---|
| safepoint poll + back edge | 3 | single-pass pays them once per FOUR iterations |
| explicit `test rax,rax` / `je` receiver null check | 2 | single-pass folds it into the header read it needs anyway |
| **phi edge copies staged through RAX** | **2** | **nobody — this is the one below** |

The first is unrolling, which the optimizing tier does not do and which
`docs/JIT_OPTIMIZATION.md` already prices at about 1.12x. The second is
`CRATONVM_JIT_IR_THIS_NONNULL`, built, default OFF, and measured *slower* — a
result that page flags as the most suspicious thing on it. The third is
avoidable outright, and is what this page is about.

## 5. The change

`Lowerer::emit_copy_op` had three steps per phi copy:

```asm
mov rax, <source>      ; read, into the move temporary
mov [dst], rax         ; the home store, when it survives
mov <phi reg>, rax     ; publish
```

RAX is a staging register, and when the phi has one of its own that register is
strictly better: the read lands there directly and the publish disappears,
because the value is already where the publish was going to put it. On
`FieldLoop.sum`'s back edge:

```asm
                            ; before                  after
35d: mov rax,r15            ; mov rax,r15 / mov r12,rax    -> mov r12,r15
360: mov rbx,[rbp-98h]      ; mov rax,[..] / mov rbx,rax   -> mov rbx,[rbp-98h]
```

**Four instructions become two**, once per iteration, and the loop preheader's
two publishes fold the same way.

### Why it is the same program

The write to the phi's register moves earlier **inside one `CopyOp`** — from
after the home store to before it — and the only instruction it crosses is that
copy's own store, whose source it now is. Relative to every OTHER copy on the
edge the ordering is unchanged, because the old publish was already inside this
op and therefore already ahead of the next op's read.
`resolve_parallel_copy`'s invariant — every source is read before anything
writes it — is a statement about that cross-op order and is untouched.

### Engagement

| method | loop-carried | `DIRECT=0` | `DIRECT=1` | delta |
|---|---:|---:|---:|---:|
| `FieldLoop.sum` | 2 | 1071 B | **1059 B** | −12 B (4 copies) |
| `FieldLoop.sumWide` | 5 | 1919 B | **1895 B** | −24 B (8 copies) |
| `PhiSwapLoop.rot3` | 3 | 1279 B | **1255 B** | −24 B (8 copies) |

The accounting closes: each folded copy removes one three-byte `mov <reg>, rax`,
and a method's copies are its GP-resident phis times its edges into the header.

## 6. Measured

`tools/tier-ab/cpu-ab.ps1` (see §8), one release binary, both arms under
`CRATONVM_JIT_FORCE_C2=1`, 10 rounds, `probe.reps=25000 probe.n=20000`.
**Three invocations, because §5.2 of the GP-register page says a few-percent
claim needs invocations rather than a tighter floor:**

| invocation | A (`=0`) | C (control) | B (`=1`) | floor | effect |
|---|---:|---:|---:|---:|---:|
| 1 | 1.063 s | 1.102 s | 0.969 s | 3.6% | **−10.5%** |
| 2 | — | — | — | 0.7% | **−7.3%** |
| 3 | — | — | — | 2.4% | **−4.8%** |

All three agree on the sign and all three are outside their own floor; the
spread across invocations (5.7 points) is the quantity §5.2 says to report, and
the honest reading is **about 1.08x, somewhere between 1.05x and 1.12x**.
Checksums identical in every run of all three.

And the tier comparison, retaken with the change in, twice:

| | before | after (1) | after (2) |
|---|---:|---:|---:|
| optimizing vs single-pass | 1.208x | **1.102x** | **1.112x** |
| floor | 2.0% | 3.8% | 2.5% |

**Roughly half the remaining inversion on this shape**, and the two post-change
invocations agree to one point.

### The shape it does NOT help, which engages twice as hard

`FieldLoop.sumWide` — the same loop with four independent accumulators —
folds **eight** copies to `sum`'s four and measures **+1.5% against a 3.0%
floor: UNMEASURABLE**. That is not a contradiction, it is the same fact from
the other side: `sumWide` does four times the arithmetic per iteration, so two
instructions are a quarter of the share they are in `sum`, and its independent
accumulators overlap what is left. A per-iteration saving is worth what the
iteration costs.

Recorded rather than omitted, because a page that reports only the shape its
change flatters is the failure mode this directory keeps writing down.

## 7. Two mutations that did not fail, and what they changed

The first version of this change staged in the phi's register whether or not
the phi's home store survived, and wrote that home **from the staged register**.
Two mutation checks on that store:

* `store_abi_reg(RAX, dst)` — store whatever was last in RAX instead of the
  value. **Every test stayed green.**
* `panic!()` — **every test stayed green**: 2356 `cratonvm-jit` unit tests, the
  145 `ir_vs_singlepass` differential tests, all of it. The branch was
  unreachable from the entire crate.

Both have one cause, and it is structural rather than unlucky. `stage` and
`drop_home` ask overlapping questions — both want a phi at this destination and
a publish that is not deferred — and differ only in `assigned_gpr(phi).is_some()`
against `home_dropped[phi]`. On every loop small enough to write as a fixture a
resident phi owns its register exclusively, `deopt_nameable` is therefore true,
`phi_home_droppable` clears it, and `stage` implies `drop_home`. Forcing the
store to run with `CRATONVM_JIT_IR_DROP_PHI_HOME=0` reaches the branch but still
cannot fail the first mutation: **nothing reads a resident phi's home word
back**, so storing the wrong register is unobservable from any answer the body
can produce.

**So the change was narrowed rather than the test weakened.** `stage` now
requires the home to be dropped, which makes the staged store unreachable by
construction (a `debug_assert` says so), and
`a_phi_copy_that_keeps_its_home_is_byte_identical` pins the exclusion the only
way that is decisive — with the home kept, the two arms must emit **identical
bytes**. The cost is one instruction on a path nothing in the crate reaches;
what is bought is that every remaining path is one this suite can fail.

The two tests are deliberately opposed, so neither can pass vacuously: one
demands `direct < plain`, the other demands `direct == plain`, and they differ
in exactly one switch.

## 8. `cpu-ab.ps1`, because `cpu-ab.sh` cannot run here

`tools/tier-ab/cpu-ab.sh` is this repo's answer to a busy host, and the carry
page's §8 is the worked example: the same 1.009x effect that `flag-ab.sh`
reported UNMEASURABLE twice with the sign flipping reproduced at a 0.1% floor
under user CPU.

**It cannot run on Windows, and it does not say so.** It reads user CPU through
`/usr/bin/time -f '%U'`, which is GNU coreutils and is absent from Git Bash, so
every sample becomes a `RUNFAIL` and the script reports nothing rather than
failing loudly. That gap is worse than a missing convenience:
`CRATONVM_JIT_IR_GP_WIDE` is a silent no-op on System V, so the register-file
question can **only** be asked on Win64 — the one platform with no user-CPU
instrument.

`tools/tier-ab/cpu-ab.ps1` is the same method through
`System.Diagnostics.Process.UserProcessorTime`: interleaved ABBA/BAAB, a
same-config control every round, checksums compared across every run, medians,
and `UNMEASURABLE` for anything inside the floor. It does tier A/B (`-Tier`) and
flag A/B (`-Flag`) from one binary. Three things it had to get right that the
shell version does not face, recorded because each cost a run:

* **Windows PowerShell 5.1 is the desktop framework.** `ProcessStartInfo` has
  `Arguments` (a string), not the `ArgumentList` collection — `ArgumentList` is
  .NET Core only, and is `$null` here.
* **`$base` silently rebinds the `-Base` parameter.** PowerShell variable names
  are case-insensitive, so a local `$base` holding a mean becomes the string
  array the caller passed, and the next division fails with
  `String[] does not contain a method named 'op_Division'`.
* **`powershell -File` binds a comma-separated argument as one string**, so `-D`
  splits on commas itself; and the results table is formatted in the invariant
  culture, because a decimal comma is a trap for whoever pastes it into a
  markdown file.

## 9. What is still owed

1. **A real application.** Every number here is a probe. The change is
   per-loop-carried-value-per-iteration and should show up wherever the
   optimizing tier compiles a counted loop, which is exactly the claim a
   workload measurement would test.
2. **The two larger items the budget in §4 names**, in order of size: the
   optimizing tier does not unroll (~1.12x on this shape, on record), and its
   receiver null check is explicit where the single-pass tier's is implicit
   (`CRATONVM_JIT_IR_THIS_NONNULL`, built, default OFF, and measured *slower* —
   which is still the most suspicious result in `JIT_OPTIMIZATION.md`).
3. **A per-op breakdown of `producer_arm`** — 16.2% of all pairing windows and
   the only row the pass can act on. `Op::Load` is the conspicuous exclusion and
   the one whose reason is textual rather than semantic; whether it is 10 of
   those 5,560 or 4,000 of them is one more counter and nobody has it.
4. **The census on a real application.** §1's probe-set totals are 188 probes,
   which is a corpus of kernels. Whether `multi_use` stays at 78% on Spring or
   H2 — where the loops are shorter and the straight-line code longer — is a
   different question and the totals cannot answer it.

## 10. Reproducing

```bash
cargo build --release -p cratonvm-cli
javac -d /tmp/pc probes/FieldLoop.java

# section 1 — the census, per method
CRATONVM_JIT=force-c2 CRATONVM_DBG_IR_LINEAR_SCAN=1 \
  ./target/release/cratonvm -cp /tmp/pc -Dprobe.reps=200 FieldLoop 2>&1 \
  | grep 'operand pairing'

# section 1 — process-wide, for a workload rather than a method
CRATONVM_JIT=force-c2 CRATONVM_DBG=jitc \
  ./target/release/cratonvm -cp /tmp/pc FieldLoop 2>&1 | grep 'operand pairing'

# section 5 — the emitted code, both arms from one binary
for F in 1 0; do
  CRATONVM_JIT_IR_PHI_COPY_DIRECT=$F CRATONVM_JIT=force-c2 \
  CRATONVM_DBG=jit-disasm CRATONVM_DBG_JIT_DISASM=FieldLoop.sum \
    ./target/release/cratonvm -cp /tmp/pc -Dprobe.reps=9000 -Dprobe.n=300 FieldLoop 2>&1 \
    | grep -m1 'full/ir FieldLoop.sum'
done
```

```powershell
# section 6 — the throughput, on Windows
powershell -File tools/tier-ab/cpu-ab.ps1 -Exe .\cratonvm.exe -Cp .\pc `
    -Class FieldLoop -Flag CRATONVM_JIT_IR_PHI_COPY_DIRECT -Rounds 10 `
    -D probe.reps=25000,probe.n=20000

# section 2 / section 6 — the tier comparison
powershell -File tools/tier-ab/cpu-ab.ps1 -Exe .\cratonvm.exe -Cp .\pc `
    -Class FieldLoop -Tier -Rounds 8 -D probe.reps=25000,probe.n=20000
```

Measurements were taken on a 32-core Windows 11 host with other tenants
building throughout; floors ranged from 0.7% to 8.5% and every run's floor is
quoted beside its effect. A floor above ~3% means the run is describing the
machine — and §5.2 of the GP-register page is why that is a necessary condition
and not a sufficient one.

## 11. Green

* 2356 `cratonvm-jit` unit tests, 145 `ir_vs_singlepass` differential tests,
  2641 `cratonvm-vm` unit tests, 607 `cratonvm-types`.
* **Regression suite 92/92** against HotSpot (`CV=... JDK=... regression-suite/run.sh`).
* The flag surface gates the pre-push hook runs — `flag_declaration_guard`,
  `flag_surface`, `flag_docs_generated` — with `docs/flag-tokens.md` and
  `docs/config/flag-inventory.md` regenerated from `INVENTORY` by
  `tools/flag-census/render-*.sh`.
* **Checksums identical in both arms** on `CratonBench` (all seven phases:
  `5000000003999999995`, `701408733`, `9592`, `173943680`,
  `1549999915000000`, `5000050000`, `68332206`) and on `CratonBenchC2` (all
  three). Those totals moved −0.4% and −0.8% respectively, which is one run
  each and **not a measurement** — recorded only as "nothing regressed". Four
  of CratonBench's seven kernels are not served by the optimizing tier at all
  (`ir-coverage-survey-20260803.md`), so an A/B of an IR-tier switch there
  compares a binary with itself, and that is the reason the throughput claim in
  §6 rests on `FieldLoop` instead.
