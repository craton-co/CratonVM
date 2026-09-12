# The optimizing tier's GP register file is not the binding constraint

**2026-09-10.** The tiering inversion on the field-read loop reproduces at
**1.594x** on current `dev`. The obvious next lever — the optimizing tier's
general-purpose register file is five registers wide while the single-pass
backend's is **seven** on Win64 — was built, engaged, and **measured slower**.
It ships default-OFF behind `CRATONVM_JIT_IR_GP_WIDE`, and this file is why.

> **RETIRED, later the same day.** Every residual this file names has been
> closed or is recorded below with what closed it, and §10 is the ledger. Two
> of them changed what this file concludes, so read §10 before quoting §2, §6
> or §7:
>
> * the census §2 quotes was **stale**, and its `droppable=0` was wrong on the
>   very compile that produced it — three homes were dropped. The line has been
>   deleted from the JIT and replaced by one that cannot drift.
> * the thesis §7 leaves standing — *"the home slot has to become optional
>   before register residency pays anything"* — is now **measurable, and not
>   supported**. On this loop the home slot is already optional for three of
>   five promoted values; turning that off changes nothing measurable, and
>   turning the whole register file off makes the loop **1% faster**.
>
> A third changed a number without changing a conclusion: §5's **+4.6% does not
> reproduce**. Four more invocations on current `dev` put the cost at about
> **1%** with an unstable sign (§5.1), and why three runs with floors of 1.0%,
> 0.5% and 0.1% can disagree by 4.5 points is §5.2 — the most portable thing
> in this file.
>
> §1's inversion and §4's engagement reproduce exactly, including on a second
> host and a second OS. The title holds — more firmly than the original
> evidence held it.

## 1. The inversion, re-measured rather than quoted

`docs/JIT_OPTIMIZATION.md` recorded the field-read loop at ~1.65x on
2026-09-03. Since then `ir-drop-home`, `ir-reg-authoritative`,
`ir-linear-scan`, `ir-deopt-regs`, `ir-phi-copy-regs`, `ir-carry-single-use`
and `ir-drop-phi-home` all went **default-ON**, and
`docs/jit/linear-scan-wiring.md` still says the linear-scan path is default
off. It is not. So the number was retaken rather than carried forward.

`probes/FieldLoop.java` `sum`, `probe.reps=25000`, one release binary, arms
interleaved BAAB/ABBA with a second copy of the baseline arm as the control,
`tools/tier-ab/tier-ab.sh`:

| arm | median |
|---|---:|
| A — baseline (`CRATONVM_C2_SUPERSEDE=0`) | 503 ms |
| C — control, identical to A | 516 ms |
| B — optimizing (`CRATONVM_JIT_FORCE_C2=1`) | **812 ms** |

Noise floor (A vs C) **2.6%**; effect **+59.4%**, i.e. **1.594x**. Checksums
identical (`acc=1500150000`) in all twenty runs. The inversion is intact, and
the register work that landed since did not close it.

**A second host, added on retirement.** Every number in this file was taken on
one contended Windows box, which is a single point of failure for a claim about
a compiler. Repeated on an 8-core Ubuntu 24.04 x86-64 host, same probe, same
script, `probe.reps=8000`:

| arm | median |
|---|---:|
| A — baseline | 117 ms |
| C — control | 118 ms |
| B — optimizing | **206 ms** |

Floor **0.9%**, effect **+75.3%**, i.e. **1.753x**, checksums identical
(`acc=480150000`). The inversion is not a property of the Windows box, and it
is *larger* on the platform the project benchmarks on.

## 2. What the census said, and why the file looked like the answer

`CRATONVM_DBG_IR_LINEAR_SCAN=1` on that method:

```text
nodes=23 positions=17 peak_live=11 scan_promoted=9
resident=5 (fp=0 gp=5) demoted=0 splits=5 scan_spills=1 scan_reloads=2
skipped: split_or_spilled=1 ... spilled=1 no_alloc=4 carried_reserved=3
home: droppable=0 blocked_deopt=3 blocked_phi=2 safepoints=16
```

> **The last line of that block was wrong when it was printed, and it is gone.**
> `home: droppable=…` came from `plan_register_residency` and asked the
> 2026-09-04 question — "promoted, named by no safepoint at all, and not a
> phi" — which `ir-reg-authoritative`, `ir-drop-home` and `ir-drop-phi-home`
> had since stopped being the question the emission asks. The same compile
> reports, from the emission itself:
>
> ```text
> homes: dropped_values=3 stores_skipped=5 read_refusals=0
> homes kept: switch=0 deopt=0 type=0 op=2
> ```
>
> **Three of the five promoted values have no home store**, and the two that do
> are not blocked by a deopt or by a phi — they are blocked because their
> defining arm does not write its home through one `store_rax` (`op`). Nothing
> in §2's argument depends on the retired line: `resident=5` against a file of
> five is unchanged, and it is still a file exhausted rather than a policy
> declining. §6 and §7 did depend on it, and say so where they do.

`resident=5` against a file of five: **every register handed out**, with
`peak_live=11`. That is a file exhausted, not a policy declining — which is a
different diagnosis from the four levers tried on 2026-09-03 (split residency,
implicit null checks, loop-weighted use counts, parameter prologue copies),
every one of which measured zero because the allocator had already declined
the values they argued about.

## 3. Two of seven registers were being left on the table, and the reason was a wrong comment

`regalloc::xmm_roles::IR_GP_LINEAR_SCAN` was `[3, 12, 13, 14, 15]` — RBX and
R12–R15 — on **both** platforms, and its doc gave the reason:

> **Callee-saved on both ABIs.** […] That rules out every caller-saved
> register, including the otherwise obvious System V candidates RSI/RDI.

That is a System V fact stated as an ABI-independent one. **Win64 makes RSI and
RDI callee-saved.** The single-pass backend has known this all along —
`x64::LOCAL_REGS` is `[u8; 7]` on Windows and `[u8; 5]` elsewhere — and
`RegFile::x86_64()` already builds them with `caller_saved: false`. Only the IR
tier's own constant was flat.

Nor were they otherwise spoken for on Windows: `CALL_ARG_REGS`,
`ENTRY_ABI_REGS` and `DEOPT_ARG0`/`1`/`2` are all `#[cfg]`-selected, and the
Windows arms are RCX/RDX/R8/R9. RSI/RDI are argument registers **only** on
System V — exactly the platform where they are also caller-saved. So the file
widens on Windows and does not move on System V, the same split
`IR_PROLOGUE_SAVED` already makes for XMM6/XMM7.

That disjointness is now a test rather than the paragraph above:
`the_gp_file_names_no_register_this_emitter_reserves` fails on either platform
if the file ever names a reserved register, and it was mutation-tested (putting
RCX in the file fails it) rather than assumed to be non-vacuous.

## 4. It engages

| | `GP_WIDE=0` | `GP_WIDE=1` |
|---|---:|---:|
| `sum` — resident | 5 (gp=5) | **6** |
| `sum` — splits | 5 | **2** |
| `sum` — scan_spills / reloads | 1 / 2 | **0 / 0** |
| `sum` — homes dropped | 3 | **4** |
| `sumWide` — resident | 5 | **7** |
| `sumWide` — splits | 27 | **14** |
| `sumWide` — spills / reloads | 10 / 14 | **7 / 9** |

More values resident, half the live-range splits, spills and reloads gone on
the narrow loop. On every census axis this is the change working.

## 5. And it is slower — by 4.6%, which §5.1 retires and 1% replaces

`flag-ab.sh`, one binary, `CRATONVM_JIT_FORCE_C2=1` in both arms,
`probe.reps=25000`:

| arm | median |
|---|---:|
| A — `GP_WIDE=0` | 909 ms |
| C — control, identical to A | 903 ms |
| B — `GP_WIDE=1` | **948 ms** |

Noise floor **0.7%**, effect **+4.6%** — above the floor, checksums identical.
**Better residency, worse code.**

The frame is not the explanation, and that was designed in: `ir_gp_file()`
narrows the *handout*, and until the default flipped it did not narrow the
*reservation*, so the arms above were measured over identical frame layouts.
(The reservation now follows the handout as well, so that the OFF arm is
byte-identical to the pre-change tree — but the measurement that produced this
number was taken with both arms reserving seven.)

A second shape, `sumWide` (four independent accumulators — the shape a wider
file is *for*, and where residency went 5 → 7), read **+3.1% against an 8.8%
floor: UNMEASURABLE.** The host had gone busy; that run says nothing, and is
recorded so nobody reads its silence as agreement.

**Retaken on retirement, and this time it says something.** One release binary
of the retirement branch, 14 rounds, `probe.reps=20000 probe.wide=true`:

| arm | median |
|---|---:|
| A — `GP_WIDE=0` | 1927 ms |
| C — control | 1903.5 ms |
| B — `GP_WIDE=1` | 1911.5 ms |

Floor **1.2%**, effect **−0.2%: UNMEASURABLE**, checksums identical
(`acc=4800600000`). So the shape a wider file is *for* — four independent
accumulators, residency 5 → 7, splits 27 → 14, spills/reloads 10/14 → 7/9 —
comes out **level**. Not slower, not faster. Two registers that halve the split
count on the one loop shape built to reward them buy nothing measurable.

(A first attempt at this read −1.4% against a **7.3%** floor and is discarded,
recorded here rather than omitted for the same reason the 8.8% one was.)

The retake was taken under the CURRENT wiring, where the OFF arm reserves five
slots and the ON arm seven — so its arms differ by 16 frame bytes as well as by
residency, which the paragraph above says was not true of the 4.6%. That can
only flatter the OFF arm, so it cannot rescue the ON one.

**On retirement: this experiment is Windows-only, and no longer reproducible as
run.** Two reasons, both worth stating rather than leaving for the next person
to rediscover.

`IR_GP_LINEAR_SCAN` is five registers on System V and `IR_GP_LINEAR_SCAN_NARROW`
is five, so `ir_gp_file()` returns the same slice in both arms there:
`CRATONVM_JIT_IR_GP_WIDE=1` is a **silent no-op on Linux**, confirmed by
running it — byte-identical census, `resident=5 (fp=0 gp=5)` either way. The
question this file asks can only be asked on Win64.

And the arms are no longer frame-identical: `ir_saved_gpr_bytes` now sizes the
save area from `ir_gp_file()`, so the 4.6% above stands as a number taken under
the older wiring and any re-run is a different measurement rather than a
confirmation of this one.

So the honest summary was: **one clean measurement saying slower, one noisy
measurement saying nothing.** Not "slower" as a settled fact — but nowhere is
there a measurement saying this pays.

### 5.1 On retirement: four more runs of `sum`, and the 4.6% does not survive

`sum` was re-run four times on current `dev`, one release binary, 14 rounds
each, `probe.reps=25000`, everything else as above:

| run | A | C | B | floor | effect |
|---|---:|---:|---:|---:|---:|
| 1 | 889.5 | 961.0 | 951.0 | **7.7%** | +2.8% — DISCARDED, the floor is the machine |
| 2 | 913.0 | 922.0 | 886.5 | 1.0% | **−3.4% — ON FASTER** |
| 3 | 925.5 | 921.0 | 933.5 | 0.5% | +1.1% — ON slower |
| 4 | 800.0 | 799.5 | 811.5 | 0.1% | +1.5% — ON slower |

Three usable runs, floors of 1.0%, 0.5% and 0.1%, and they do not agree on the
**sign**. Two say about +1.3% slower; one says 3.4% faster. Nothing in the
emission moved between them — the census is identical in all four (§4), and the
checksums are identical in all 168 runs.

**What that retires is the number, not the conclusion.** "+4.6% slower" was one
invocation. Re-run four times against a body whose layout-epoch guard is now one
instruction instead of three, the same lever lands somewhere in ±3.5% with an
unstable sign, and its central tendency is about **1% slower**. So:

* nowhere, in six invocations across two shapes, is there a measurement saying
  a wider file **pays**;
* and the honest magnitude of the cost is ~1%, not 4.6%.

### 5.2 The methodological finding, which outlives this flag

Runs 3 and 4 had floors of **0.5%** and **0.1%** — as clean as this apparatus
ever reports — and run 2's floor was 1.0%. They disagree by 4.5 percentage
points. **A control arm measured inside one `flag-ab.sh` invocation bounds the
drift within that invocation and says nothing about the drift between
invocations.** Every A/B in this file, including its headline, is one
invocation.

That is not an argument for distrusting the apparatus; interleaving and a
control are what make a single run readable at all. It is an argument that
**a few-percent effect needs repeated invocations, not a tighter floor** —
`tools/tier-ab/README.md`'s "a floor above ~3% means the run is describing the
machine" is a necessary condition and not a sufficient one. The 1.594x
inversion in §1 is nowhere near this regime and is unaffected; a 4.6% claim is
squarely inside it, and was.

## 6. Why more registers do not pay, which is the part worth keeping

`lower_data_node` gives every value a frame slot and **writes it**, and the
residency file is a read cache layered on top. `linear-scan-wiring.md` states
the consequence from the allocator's side — *"It buys loads, not stores. The
store side is still one per definition. That is the honest ceiling of this
increment."* — and `home: droppable=0` on this method says the ceiling is
exactly where the loop is.

> **Corrected on retirement.** The quoted sentence is right; the evidence
> cited under it was not. `home: droppable=0` was a stale diagnostic (§2), and on
> this method the emission drops **three** of five homes and skips five stores.
> So the ceiling is not "every value writes its home" — it is "every value the
> allocator promotes but the droppability predicates decline still writes its
> home", which on this loop is two values, blocked by the shape of their
> defining arm rather than by a deopt.
>
> The paragraph below therefore has to be read per value, not per method: for a
> value whose home survives, promoting it adds a publish and removes no store,
> which is exactly the cost curve described. For the three whose home does not
> survive, the store IS removed — and the loop is still slower, which is the
> finding §10 turns into a measurement.

Promoting a value therefore **adds** its publish move and **removes** no store.
A wider file buys more publishes against the same store traffic, and when the
extra candidates are ones the allocator then splits (`splits=14` even in the
best arm), it buys reload and copy instructions too. That is a cost curve that
gets worse with file width, not better, and it is why the fifth attempt in
`JIT_OPTIMIZATION.md` — a register-to-register publish — produced
byte-identical code: *"no policy change on the cache can remove a store the
model emits unconditionally."*

This measurement is that thesis approached from the opposite direction. The
earlier work showed residency cannot be *increased* usefully one value at a
time. This shows it cannot be increased usefully *in bulk* either, while the
frame stays authoritative.

## 7. What this retires, and what it leaves

**Retired: "the optimizing tier is short of registers."** It is short of two
registers on Win64 and giving them to it is measurably not an improvement. Any
future argument of the form "widen the file" needs to answer this measurement
first.

**Left standing, and now with one more independent witness:** the home slot has
to become optional before register residency pays anything. That is the change
`JIT_OPTIMIZATION.md` calls "an architecture change, not another heuristic" —
`emit_safepoint_map`, `build_deopt_points` and `emit_phi_copies` all have to be
able to read a **register**, which needs a register bank in the oop map and in
the deopt frame reconstructor. `ir-drop-home` and `ir-deopt-regs` are the first
two pieces of that and they are already on; `droppable=0` on this loop says
they do not yet reach a loop-carried value whose home a deopt or a phi still
names (`blocked_deopt=3 blocked_phi=2`).

> **The last sentence is false and the one before it is now testable.** Both
> rest on the retired census. What the emission actually does on this loop is
> `dropped_values=3` of five promoted, with the remaining two blocked by
> `op` — the store shape of their defining arm — and by **neither** a deopt nor
> a phi. Home elimination reaches this loop already.
>
> Which turns "the home slot has to become optional before residency pays"
> from a prediction into an A/B, and §10 runs it. It does not pay: on this loop
> turning home dropping off is **UNMEASURABLE** and turning the register file
> off entirely is **1% FASTER**. The architecture change may still be right —
> three of five is not all of five — but it can no longer be quoted as the
> thing this measurement is waiting for.

**The flag is the instrument for asking again.** When home elimination reaches
the point where `droppable` is non-zero on a counted loop,
`CRATONVM_JIT_IR_GP_WIDE=1` re-asks this question in one run, and the two
numbers above are what it has to beat.

> **The trigger has already fired, and the answer did not change.** Homes are
> dropped on this loop today (three of five, four of six with the flag on), so
> the condition above is met. `CRATONVM_JIT_IR_GP_WIDE=1` still promotes more,
> splits less and drops MORE homes — and still is not faster. The flag remains
> the instrument; what it is waiting for is a *different* trigger, and §10.2
> says why the obvious one is not it.

## 8. A lead this turned up and did not chase: the layout epoch guard is loop-invariant

Counted off the `full/ir` disassembly of `FieldLoop.sum` taken for §2
(`CRATONVM_DBG=jit-disasm CRATONVM_DBG_JIT_DISASM=FieldLoop.sum`), the loop
body runs from the back-edge target `0x1d1` to the back edge at `0x386` and its
hot path is about **27 instructions**. The first four of them are
`ir_lower::emit_layout_epoch_guard`:

```text
1d1: mov  r11, 7FF75BB18AF4h   ; &layout_replace_epoch  -- 10 bytes of imm64
1db: mov  ecx, [r11]
1de: cmp  ecx, 0
1e4: jne  -> the jit_getfield helper
```

That is **~15% of the loop's instruction count and 25 of its bytes, per
iteration**, spent re-reading a **process-wide** counter that the guard's own
doc says "only a REPLACEMENT bumps — never a new class registration". It is
loop-invariant by construction, and the value it guards (a baked compact cell
offset) is a compile-time constant.

Two things make this more interesting than the register file:

* it is paid by **every inline field access in every method**, not only by
  loops, so it is not a loop-shape-specific lever;
* the single-pass tier's own body of this method (`osr/sp`, 4x unrolled) shows
  the same guard, so this is not a tier gap — it is a cost both tiers carry and
  neither hoists.

Three routes, cheapest first, none of them measured here:

1. **RIP-relative the compare.** `cmp dword [rip+disp32], imm32` is one
   instruction and ~10 bytes against four instructions and 25, and drops the
   R11 clobber. The back-edge poll already emits a RIP-relative `test byte
   [rel …]`, so the encoder can do it; the constraint is the ±2GB reach from
   the code buffer to the static, which needs a fallback to the imm64 form when
   it does not hold.
2. **Hoist it to the loop preheader.** Sound only if a layout replacement
   cannot be *applied* between the preheader and a given iteration — i.e. only
   with a re-check on the safepoint poll's slow path, which is where a
   replacement would land.
3. **Make it a dependency instead of a check.** Register the artifact against
   the layout epoch and let the existing `jit_cache.remove` /
   `invalidate_matching` path drop it on a bump, the way HotSpot handles an
   assumption. That deletes the guard from the body entirely and is the largest
   of the three.

**Chased the same day.** Route 1 landed and is worth **14.5%** on a four-site
loop against a 2.5% floor (and nothing on a one-site loop, which is how a
per-site cost is supposed to behave). Routes 2 and 3 are **refused**: both fail
on in-flight frames, because cache invalidation governs future entries to a
method and not a frame already executing one — the same fact that retired
artifact displacement. Full write-up, including the address measurement that
explains why the obvious encoding had never worked, in
[`c2-the-layout-epoch-guard-was-unreachable-by-rip-20260910.md`](c2-the-layout-epoch-guard-was-unreachable-by-rip-20260910.md).

## 9. Reproducing

```bash
cargo build --release -p cratonvm-cli
javac -d /tmp/pc probes/FieldLoop.java

# §1 — the inversion
bash tools/tier-ab/tier-ab.sh -Exe "$PWD/target/release/cratonvm" \
    -Cp /tmp/pc -Class FieldLoop -Rounds 5 -D probe.reps=25000

# §4 — engagement
CRATONVM_JIT_IR_GP_WIDE=1 CRATONVM_JIT_FORCE_C2=1 CRATONVM_DBG_IR_LINEAR_SCAN=1 \
  ./target/release/cratonvm -cp /tmp/pc -Dprobe.reps=200 FieldLoop 2>&1 | grep ir-ls

# §5 — the result
bash tools/tier-ab/flag-ab.sh -Exe "$PWD/target/release/cratonvm" \
    -Cp /tmp/pc -Class FieldLoop -Flag CRATONVM_JIT_IR_GP_WIDE \
    -Base "CRATONVM_JIT_FORCE_C2=1" -Rounds 6 -D probe.reps=25000

# §5 — the `sumWide` companion, which needs the shape selector
bash tools/tier-ab/flag-ab.sh -Exe "$PWD/target/release/cratonvm" \
    -Cp /tmp/pc -Class FieldLoop -Flag CRATONVM_JIT_IR_GP_WIDE \
    -Base "CRATONVM_JIT_FORCE_C2=1" -Rounds 14 \
    -D probe.reps=20000 -D probe.wide=true

# §10.2 — the two levers §7 said this was waiting on
for f in CRATONVM_JIT_IR_DROP_HOME CRATONVM_JIT_IR_LINEAR_SCAN; do
  bash tools/tier-ab/flag-ab.sh -Exe "$PWD/target/release/cratonvm" \
      -Cp /tmp/pc -Class FieldLoop -Flag $f \
      -Base "CRATONVM_JIT_FORCE_C2=1" -Rounds 8 -D probe.reps=8000
done
```

**On System V, §4 and §5 are inert** — `CRATONVM_JIT_IR_GP_WIDE` is a no-op
there (§5). §1 and §10.2 run on either platform, and §10.2's numbers are
Linux ones.

The original A/B numbers were taken on a contended 32-core Windows host; §1's
floor was 2.6% and §5's 0.7%, and §5's `sumWide` companion run's was 8.8%,
which is what disqualified it. A floor above ~3% means the run is describing
the machine — **and §5.2 is why that is a necessary condition and not a
sufficient one.** Anything claiming a few percent wants several invocations,
not one clean one.

§1's Linux re-measurement and §10.2 were taken on an 8-core Ubuntu 24.04 host
at a load average under 2; the same host at 20+ produced three floors between
3.8% and 18.2% on the same probe, which is what the load average is for.

## 10. Retirement ledger

Written on closing this file. Every residual it left, what happened to it, and
the measurements that were run because retiring it required them.

### 10.1 The diagnostic this file quoted was stale, and the zero was wrong

`[ir-ls] home: droppable=… blocked_deopt=… blocked_phi=…` came from
`plan_register_residency` and asked one question — *promoted, named by no
safepoint at all, and not a phi*. That was the right question on 2026-09-04,
when the only way to lose a home was whole-body trap freedom. `ir-deopt-regs`,
`ir-reg-authoritative`, `ir-phi-copy-regs` and `ir-drop-phi-home` then widened
the rule the EMISSION uses to *named by no REACHABLE frame state*, and the
census did not follow. By the time this file quoted it, it printed
`droppable=0` on a compile that dropped three homes.

The line is deleted. `Lowerer::census_home_blocks` replaces it, and it is
computed from `home_dropped` itself — the array the emission actually consults
— under an accounting identity (`dropped + switch + deopt + type + op ==
promoted`, `debug_assert`ed) so that a clause added to `value_home_droppable`
and not to the census fails `cargo test` instead of quietly reading low. It
prints as `[ir-ls] homes kept:`, beside the `dropped_values` it explains.

That is the general lesson and it is worth more than this file's number: **a
census that computes its own answer instead of reading the one the code used
will drift, and it will drift towards zero**, because the widening it misses is
always in the permissive direction. Ties to the outcome, or delete.

### 10.2 The claim §7 left standing is now measurable, and it does not hold yet

§7 says register residency cannot pay until the home slot is optional. On
`FieldLoop.sum` the home slot **is** optional for three of five promoted values
already, so the antecedent is satisfied enough to test the consequent. Both
arms from one binary, `CRATONVM_JIT_FORCE_C2=1` in the base, 8 rounds,
`probe.reps=8000`, on an 8-core Ubuntu 24.04 host:

| question | flag | A | C | B | floor | effect |
|---|---|---:|---:|---:|---:|---:|
| does dropping home stores pay? | `IR_DROP_HOME` | 210.5 ms | 215.0 ms | 213.0 ms | 2.1% | +0.1% — **UNMEASURABLE** |
| does the register file pay at all? | `IR_LINEAR_SCAN` | 205 ms | 205 ms | 207 ms | 0.0% | **+1.0% — ON is SLOWER** |

Checksums identical in every run (`acc=480150000`).

The second row is the one to keep. This file was written about widening the GP
file; the file **itself** — the whole linear-scan register cache, FP and GP,
default-ON since 2026-09-02 — costs 1% on the loop the tier inversion is about.
So the finding is not "the extra two registers do not pay". It is **the
register file does not pay here, at any width**, and neither does removing
three of its five home stores.

That does not make the flags wrong to have on: `BinTrees.itemCheck` promotes
seven values with no demotions, and this is one loop. It does retire the
sentence that residency is waiting on home-slot elimination. It is not waiting
on that; on this shape it is not paying for a reason neither lever reaches.

**Both rows are single invocations, and §5.2 says what that is worth at this
magnitude.** The `IR_DROP_HOME` row is a verdict of UNMEASURABLE, which
repeated invocations can only sharpen, not reverse. The `IR_LINEAR_SCAN` row's
+1.0% should be read as "not an improvement", not as "a 1% cost" — the claim
this section rests on is that neither lever produces the improvement §7
predicted, and that survives the objection. A confirmed magnitude would want
three invocations, as §5.1 gave the flag this file is named for.

### 10.3 The Win64 experiment, retaken six times

`CRATONVM_JIT_IR_GP_WIDE` is a **silent no-op on System V** —
`IR_GP_LINEAR_SCAN` is five registers there and `IR_GP_LINEAR_SCAN_NARROW` is
five — confirmed by running it: byte-identical census in both arms. So the
question is Win64's alone, and both shapes were retaken there on one release
binary of this branch:

* **`sumWide`** — 14 rounds, floor 1.2%: **−0.2%, UNMEASURABLE**. Level. The
  8.8%-floor run this file could not use is now answered.
* **`sum`** — four invocations, 14 rounds each. One discarded on a 7.7% floor;
  the other three read −3.4%, +1.1% and +1.5% against floors of 1.0%, 0.5% and
  0.1%. **The +4.6% does not reproduce**; the cost is about 1%, and one run in
  three disagreed even about the sign.

The flag stays default OFF, and now for a better-supported reason than it had:
not "it is 4.6% slower" but "in six invocations across two shapes, none of them
says it pays, and the best estimate of the cost is a percent."

§5.2 is the part of this worth carrying elsewhere: three runs whose own control
arms agreed to 0.1–1.0% disagreed with each other by 4.5 points, which is what
a within-invocation floor is and is not evidence of.

### 10.4 Documents this file said were wrong, now fixed

* `docs/jit/linear-scan-wiring.md` — §1 of this file flagged it as still saying
  the path is default off. Every stale sentence in it now carries a dated
  correction beside what it used to say, including the one that mattered: its
  "no reference can be register-resident, because there is no GP register"
  argument is gone, and the property now rests on `plan_register_residency`
  refusing `IrType::Ref`.
* `docs/JIT_OPTIMIZATION.md` — carried the exact wrong claim §3 retired
  ("callee-saved on **both** ABIs … rules out even the otherwise-obvious System
  V candidates RSI/RDI"), and quoted the field-read loop at ~1.65x from
  2026-09-03. Both corrected in place, dated.
* `docs/feature-designs/ir-optional-home-slot.md` — read `droppable=0` in the
  present tense. Corrected.

### 10.5 The lead in §8 went further than §8 says, and paid better

Route 1 landed on Windows and is worth 14.5% there. On **Linux it never engaged
at all** — the counter is in the VM heap near 2 TB and the JIT's code buffer is
`mmap`'d near 130 TB, so every guard *and* every back-edge safepoint poll took
the long form on every compile, before and after the change. The fallback was
not a safety net; on that platform it was the whole implementation.

That is a placement problem rather than an encoder one, and it now has a lever.
`CRATONVM_JIT_CODE_NEAR_GLOBALS=1` (`jit::platform`, default OFF, Unix only)
gives `mmap` an address hint derived from the epoch counter — a hint and
nothing more, no `MAP_FIXED`, so it cannot overlap the heap or anything the GC
reads. With it on, the same body goes from 0 short-form guards and 0 short-form
polls to **4 and 2**, loses exactly 50 bytes (4 x 9 + 2 x 7), and
`MultiFieldLoop` runs **7.0% faster against a 0.8% floor** — repeated on a
saturated host it reads −3.6% against a 2.5% floor, so two invocations agree on
the sign, which §5.2 says is the bar. The fast regression suite is **92/92**
with it on and `cargo test -p cratonvm-jit` is 2347/2347. The one-site
companion is unresolved and recorded as such.

The finding that made it work is worth as much as the number:
`/proc/<pid>/maps` shows mimalloc holding **one 16GB reservation** with the
anchor 511MB inside it, so a hint walk that only goes *up* from the anchor is
inside that mapping for the entire reachable window. The room is underneath the
arena's base. Full write-up in
[`c2-the-layout-epoch-guard-was-unreachable-by-rip-20260910.md`](c2-the-layout-epoch-guard-was-unreachable-by-rip-20260910.md).

**Chased to the end on 2026-09-11, and both of this row's open ends are
closed.** That page's own residuals were the last ones either file had:

* **The one-site companion is resolved**, and so is the four-site number it was
  supposed to scale against. On an idle host the flag is worth **−11.4% to
  −12.3%** on `MultiFieldLoop` across four invocations whose floors are all
  under 1.3%, and **−1% to −3%** on `FieldLoop` — one guard against four. The
  7.0% this row quotes was measured on a busy box against a 1.7x slower
  baseline; it is the same effect over a different denominator.
* **The differential run the flag's default was waiting on has been run**:
  the whole Spring index (2848 classes) and the whole H2 index (218), both arms
  from one binary. Twenty-seven classes differed; **none survives re-running**,
  and the re-run is the finding — the two arms had been run concurrently on
  suites that bind ports and temp directories.
* **The flag still ships OFF**, and the reason is new rather than a lack of
  evidence: setting it makes `alloc_code_adjacent_cell` decline for the whole
  process, so on a host where the placement ladder misses, the safepoint poll
  loses the short form it has by construction today. That page names the two
  changes that retire the objection.

That page also carries the methodological row worth reading beside §5.2 of this
one: **fifteen invocations of the same A/B on a loaded box**, one of which
produced the tightest floor of the entire sequence — 0.9% — **with the wrong
sign**. Here it was three runs disagreeing by 4.5 points; there it is nineteen.
