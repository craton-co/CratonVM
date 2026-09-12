# The per-frame contract, lane 1: who actually pays for a home word

**2026-09-12.** [`c2-fib-per-call-budget-20260912.md`](c2-fib-per-call-budget-20260912.md)
§7 closes by naming the largest term in `fib`'s per-call budget and declining to
fix it:

> **The parameter round-trips through memory.** […] Every value has a home word
> and the home is written whether or not anything reads it. This is the single
> largest term in §2 and it is an architectural property of the lowerer, not a
> missing peephole.

This page takes that lane and reaches three results.

* **`CRATONVM_JIT_IR_RESIDENCY_CROSSBLOCK` — a win where the register file has
  slack, and a loss where it does not.** Single-use values that
  `plan_carries` provably cannot reach are refused a register by a rule that
  assumes the carry took them. Admitting exactly that complement is **7.6%
  faster on `probes/FieldLoop.java` over a 0.0% floor** with a *smaller*
  optimizing body, and free on `FibCall.fib` and `ManyExits.pick`. But on
  `probes/RegPressure.java`, which oversubscribes the five-register file, it is
  **5.4% to 12.5% SLOWER** — §6c. **Default OFF, for that measured reason.**
* **An occupancy term (`…_CROSSBLOCK_BUDGET`, default ON) — §6d.** Makes the
  admission outbid the loop-carried value it displaces. Keeps the win, makes the
  12.5% case byte-identical to not running at all — and still does not close the
  lane, because the two probes it must separate have *identical* weights and
  prices. **Loop weight is provably not the discriminating variable.**
* **A retraction.** Admitting *constants* looked equally obvious, was built,
  compiled and passed 2 393 tests, and is **structurally incapable** of paying.
  §2 is why, and the reason was already written in the tree.

The first two give a cost model the current rule does not have (§5), and that
is the useful output of the page: a promoted value costs **two memory
operations per call** — one save, one restore, *independent of the exit count* —
and repays it **per execution of the reload it removes**. So residency is
roughly free on straight-line code and pays in proportion to loop trip count.

§5 reaches that by **failing to confirm its own first model**: a third probe,
`probes/ManyExits.java`, was built specifically to be the shape where an
epilogue-scaled cost should make this flag lose, and it came out neutral twice.
The error in the first model is the useful part, and it is recorded rather than
quietly replaced.

The model is still incomplete, and §6c says how. Its benefit term assumes the
register is **free to take**; under pressure its real price is whatever the value
that would otherwise have held it was worth. That is the term that makes
`RegPressure` lose, and it is the one a successor rule has to add.

Windows dev box under unrelated load from other worktrees; every timing is
interleaved with a CONTROL arm and reports its own floor. Checksums identical on
every row of every table.

## 1. Where the home words come from

`plan_register_residency` decides which IR values live in a register. It refuses
in two places that matter here, and the census names both:

```text
[ir-ls] skipped: … const=2 single_use=13 …
```

* **`const`** — `Op::Const` is refused unconditionally, before the pays-check
  runs at all.
* **`single_use`** — `ir_residency_pays_here` reduces to `static_uses >= 2` in a
  default build, so a value read exactly once never gets a register.

Both look like missed opportunities on a method whose budget is dominated by
home stores and reloads. Exactly one of them is.

## 2. The constant arm is right, and the reason is one flag away

**Retracted hypothesis.** The reasoning that produced it was: `fib` and
`FieldLoop` both show `const=2`, a constant read every iteration reaches its use
through memory, and a constant is the *safest* value in the graph to promote —
no inputs, no aliasing, and a deopt frame already names it as a literal
(`Op::Const(v) => FrameValue::Int(v)`). Supporting evidence looked strong:
lifting the residency refusals wholesale (`CRATONVM_JIT_IR_RESIDENCY_PAYS=0`) is
5.7% faster on `FieldLoop`, and the census attributed the whole of that to one
value going `const` 2 → 0.

It was built, gated on the loop-weighted pays-check so straight-line code would
be unaffected. Both halves of that prediction were wrong.

**On `FieldLoop` it is a no-op.** The constants are admitted and then handed
straight back:

| `CRATONVM_JIT_IR_CONST_RESIDENCY` | `const` | `resident` | `split_or_spilled` | `carried_reserved` | `FieldLoop.sum` |
|---|---:|---:|---:|---:|---:|
| 0 | 2 | 5 | 1 | 4 | **len=1635** |
| 1 | **0** | 5 | 2 | 3 | **len=1635** |

`resident` does not move and the emitted body is **byte-identical**. The
allocator spills one of them and the carry takes the other. The census line that
pointed at a constant was reporting a value that was never going to reach a
register by this route.

**On `fib` it is measurably slower.** The prediction was byte-identity, because
the loop-weighted form reduces to `static_uses >= 2` at depth 0. But `fib`'s
constant `1` is read twice — by `n-1` and by the `n <= 1` test — so it clears
that bar, is admitted, and the body **grows 788 → 827 bytes**:

```asm
mov [rbp-0B0h],r12      ; NEW: a callee-saved register saved in the prologue
mov r12,[rbp-48h]       ; the constant LOADED FROM ITS HOME WORD, not materialised
mov r12,[rbp-0B0h]      ; NEW: and restored
```

```text
A (flag off)  median 107.0 ms   C (control) median 107.0 ms
B (flag on)   median 109.0 ms   n=26
noise floor 0.0%   effect +1.9%   ratio 1.019x
VERDICT: flag ON is SLOWER — above the floor
```

**The premise was wrong, and one existing flag says so.**
`CRATONVM_JIT_IR_CONST_IMM` is **default ON**: a constant is already read as an
immediate rather than from its home word. So a constant's baseline cost is
**zero memory operations**, and residency cannot improve on zero — it can only
add the register's own prologue save and restore, which is what the disassembly
above shows it doing. The `const=` skip line is not a missed opportunity; it is
the correct answer, and the `const` arm has been correct since it was written.

**The tree already said so, in the one place that had to get it right.**
`ir_reserve_carried_enabled`'s candidate filter — the *other* pass that hands
out registers from this file — excludes constants with the reason spelled out:

```rust
// A constant is materialised as an immediate by every
// reader, so its register could never be read.
&& !matches!(node.op, Op::Const(_))
```

That comment is the whole of §2, written before the experiment and by the
author of the competing pass. A grep for `Op::Const(_)` across the two
register-handout sites would have retired the hypothesis before it was built,
and that is the cheap check this page exists to recommend.

The transform is **reverted**. What survives is this section and the rule under
it: **before promoting a value, price what it costs today, not what a home word
usually costs.** A value with a rematerialisation cheaper than a load is already
free.

## 3. The single-use arm IS wrong, and the carry window says exactly where

The `single_use` line is the larger population — 13 of 20 nodes on `fib`, 15 of
23 on `FieldLoop` — and unlike the constants these values genuinely reach their
use through memory. The refusal that produces them is `static_uses >= 2`, and
the argument for it is sound *where it applies*: a single-use value costs one
publish and saves one reload, a wash — **and `Lowerer::plan_carries` already
serves those for free**, by handing the producer's RAX straight to the consumer.

But the carry scans a window exactly one node wide:

```rust
for w in 1..block.nodes.len() {
    let prod = block.nodes[w - 1];
    let cons = block.nodes[w];
```

so it reaches a single-use value only when the consumer is the very next node in
the same block. Every other single-use value gets neither mechanism: not the
carry, because the consumer is not adjacent; not residency, because the rule
that refuses it assumes the carry took it.

`CRATONVM_JIT_IR_RESIDENCY_CROSSBLOCK=1` admits exactly the complement of that
window, so the two partition the population rather than competing for it —
`plan_carries` already declines a value residency took
(`assigned_gpr(prod).is_some()`, `carry_skips[3]`), and
`the_carry_window_is_exactly_the_next_node_in_the_same_block` pins the two
definitions of the window against each other.

**On the loop it pays: 7.6% over a 0.0% floor.** Thirteen interleaved rounds,
control arm, `probe.reps=3000 probe.n=20000`:

```text
A (flag off)  median  99.0 ms    C (control) median 99.0 ms
B (flag on)   median  91.5 ms    n=26
noise floor 0.0%   effect -7.6%   ratio 0.924x
VERDICT: flag ON is FASTER — above the floor
```

and the optimizing body gets **smaller**, 1052 → 1039 bytes.

**The reason is one value, and it is the induction variable.** Normalising
addresses and diffing the two `full/ir` bodies leaves this, inside the loop:

```asm
; flag OFF                            ; flag ON
mov r14,rax
lea eax,[r15+1]                       lea r14d,[r15+1]
mov [rbp-90h],rax   ; store i+1  →    (gone)
cmp r15d,r12d                         cmp r15d,[rbp-60h]
…                                     …
mov rbx,r14                           mov rbx,r12
mov r15,[rbp-90h]   ; reload i+1 →    mov r15,r14
```

`i + 1` was computed, stored to its home word, and reloaded — **twenty thousand
times per call** — because it is read exactly once, by the phi at the loop back
edge, and that read is not the adjacent node. This is the budget page's "the
parameter round-trips through memory", in a loop, removed.

## 4. On `fib` it is free, and the static accounting looks worse than it is

Same flag, same binary, the shape the budget page opened up:

```text
A (flag off)  median 179.0 ms   C (control) median 177.0 ms
B (flag on)   median 178.0 ms   n=26
noise floor 1.1%   effect +0.0%   ratio 1.000x
VERDICT: UNMEASURABLE (0.0% effect inside a 1.1% floor)
```

It engages hard — `single_use` 13 → 6, `resident` 2 → 6 — and the body grows
**788 → 883 bytes**. The full memory-operation accounting, counted off the two
disassemblies rather than estimated:

| | stores to `[rbp]` | loads from `[rbp]` | epilogues |
|---|---:|---:|---:|
| flag off | 19 | 21 | 4 |
| flag on | **19** | **30** | 4 |

* **Stores are a wash.** Three home stores genuinely disappear (`-78h`, `-68h`,
  `-50h` — home-dropping works), and three callee-saved *saves* replace them
  (`mov [rbp-0B8h],r13`, `r14`, `r15`).
* **Loads go up by nine.** The three reloads the transform set out to remove do
  disappear, and are paid for with twelve epilogue restores — three registers
  across four exits.

Fifteen extra memory operations *emitted* for three removed, and it costs
**nothing measurable** — because the fifteen are not what runs. Three are the
prologue saves and twelve are restores spread across four epilogues, of which
**exactly one executes per call**. The dynamic cost is three saves plus three
restores, and `fib` recovers three stores and three reloads against it. A wash,
which is what the clock says.

That distinction is easy to state after the fact and this page did not have it
until §5 went looking for the shape where the epilogue count should bite. It is
worth reading §4 and §5 in that order for that reason.

## 5. The cost model, the probe built to break it, and the correction

**The residency register file is entirely callee-saved.** Five registers by
default — RBX, R12–R15 — and `plan_register_residency` says so itself: *"every
register in it is callee-saved and no fixed x86 operand names one"*. So a
promoted value takes a register that must be saved on entry and restored before
returning. `fib` has four epilogues, and each one restores the whole set:

```asm
mov rbx,[rbp-0A8h] ; mov r13,[rbp-0B8h] ; mov r14,[rbp-0C0h] ; mov r15,[rbp-0C8h]
add rsp,200h ; pop rbp ; ret          ; … and again, ×4
```

The first model this page wrote down read that as

> cost = one save plus `N_epilogues` restores, paid once per call

and predicted that a **loop-free method with more than four exits** should push
the flag negative — nothing to repay a cost that grows with the exit count.

`probes/ManyExits.java` was written to be exactly that shape and nothing else:
six values computed in the entry block, each read exactly once and each from a
different later block (so all six are `single_use`, and none is reachable by
`plan_carries`' one-node window), six returns, no loop. It compiles standalone
— `full/ir ManyExits.pick(IIII)I`, six epilogues, not inlined — and the flag
engages exactly as designed. Arm 0 uses **no** callee-saved register at all;
arm 1 takes three:

| | callee-saved saved | stores | loads | body |
|---|---:|---:|---:|---:|
| flag off | **0** | 50 | 47 | 1146 |
| flag on | **3** (RBX, R14, R15) | 46 | **63** | 1242 |

Three saves and **eighteen** restores — three registers across six exits —
against four home stores removed. On the first model that is a clear loss.

**It is not a loss. It is neutral, twice:**

```text
reps=60  n=200000  13 rounds   floor 6.6%   effect +0.7%   ratio 1.007x
reps=120 n=200000  21 rounds   floor 4.0%   effect +0.2%   ratio 1.002x
```

**The model was wrong, and the error is worth more than the model.** Eighteen
restores are emitted, but **exactly one epilogue executes per call.** The
register file is saved once in the prologue and restored once on whichever exit
is taken, whatever the static exit count. So:

> **dynamic cost** = 2 memory operations per promoted register, **per call** —
> one save, one restore — and it does **not** scale with the epilogue count.
> **static size cost** = `1 + N_epilogues` per register, which is why both
> loop-free bodies grew by ~95 bytes.
> **benefit** = reloads and home stores removed, times **how often they
> execute**.

Under the corrected model all three probes fall out of one arithmetic, and the
two neutral results are neutral for the *same* reason rather than by
coincidence:

| | epilogues | regs added | dynamic cost/call | removed | body | measured |
|---|---:|---:|---:|---:|---:|---|
| `FieldLoop.sum` | 2 | **0** (all 5 already saved) | **0** | reload+store **× 20 000** | 1052 → 1039 | **0.924x** |
| `FibCall.fib` | 4 | 3 | 6 ops | 3 stores + 3 reloads | 788 → 883 | 1.000x |
| `ManyExits.pick` | 6 | 3 | 6 ops | 4 stores + ~2 reloads | 1146 → 1242 | 1.002x |

`fib` and `ManyExits` pay six operations and recover about six: a wash, at four
exits and at six alike. `FieldLoop` pays **nothing** — its body already saves
all five registers in both arms — and recovers a load and a store on every one
of twenty thousand iterations.

So the honest statement of the trade is simpler and less alarming than the one
this page first reached: **residency is approximately free per call, and pays
exactly to the extent that the reload it removes sits inside a loop.** The
epilogue count is an icache and code-size concern, not a memory-traffic one.
There is no exit count at which this flag becomes a loser, which is a stronger
result for it than the page set out to prove — reached by trying to prove the
opposite.

## 6. What the rule should be, and why `LS_LOOP_WEIGHT` did not already do it

The shipped rule is `static_uses >= 2`. Against the corrected model that form is
right and its *inputs* are wrong twice over.

**It counts static edges, not executions.** `CRATONVM_JIT_IR_LS_LOOP_WEIGHT`
exists to fix precisely that, and it changes nothing measurable — which looks
like a refutation and is not. `residency_pays_loop_weighted` asks
`uses_freq >= def_freq * 2`, and for `FieldLoop`'s `i + 1`, **defined and used at
the same loop depth**, that is `10 >= 20` — refused. A value whose definition is
as hot as its use gets no help from loop weighting at all, and the induction
variable is the archetype of that shape. This is why the loop-weight flag and the
crossblock flag do not overlap despite appearing to address the same rule.

**It prices a publish and a reload as equal.** That is the `>= 2`: one publish
against one reload is called a wash. A publish is a register-to-register move —
frequently zero cycles under renaming — and a reload is a memory load. They are
not equal, and for a single-use value that is the entire trade. `plan_carries`
already acts on that asymmetry for adjacent consumers; the crossblock arm is the
same judgement applied to the values the carry cannot reach.

So the successor rule is not "add an epilogue term" — §5 just retired that. It is
to compare the two sides at their real prices, with `live.weight` on the benefit
side, and to stop treating a register move as costing what a load costs.

## 6a. Status

`CRATONVM_JIT_IR_RESIDENCY_CROSSBLOCK` ships **default OFF**. One win and two
neutrals across three deliberately different shapes — a counted loop, a
self-recursive call, and a six-exit branch tree — is good evidence for the
mechanism. It is **not** evidence that no losing shape exists, and §6c found
one: under register pressure the flag is 5.4–12.5% slower. Read §6c before
reading the rest of this section as encouragement.

What it still needs before the default flips:

* ~~A differential correctness soak~~ and ~~`CratonBench` /
  `CratonBenchC2` checksum parity~~ — **both done, §6b.** No divergence, and
  every checksum on both benchmarks is bit-identical.
* ~~A large real body.~~ **Done, §6c** — and the occupancy term it asks for is
  built and measured in **§6d**.
* ~~A large real body (original wording).~~ **§6c.** `probes/RegPressure.java` oversubscribes the five-register
  file, and the flag is **5.4% to 12.5% SLOWER** there, above the floor, three
  runs out of three. The default stays OFF for that reason now, not for want of
  evidence.
## 6b. The correctness evidence, and exactly how much it covers

A default-off JIT flag is flipped on evidence that turning it on changes no
answer, which is what `tools/jit-flag-soak.sh` exists to produce.

```text
FLAGSOAK CRATONVM_JIT_IR_RESIDENCY_CROSSBLOCK=1 -XX:+UseGenerationalGC:
  deterministic-and-agree=40  divergent=0  nondeterministic=0  failed-off=103
```

**`divergent=0` is the pass**, and `nondeterministic=0` means every workload that
ran was reproducible under a fixed configuration — so none of the 40 excused
itself. But that script's own instruction is to read both numbers, and
`failed-off=103` is the bound: of 143 top-level classes in
`vm/tests/resources/cratonvm`, **86 declare no `main` at all** (57 do), so most of
the corpus is structurally unrunnable as a standalone workload and 17 more fail
for their own reasons with the flag off. **This soak testifies about 40
workloads, not 143.** It is evidence, and it is not a wide net.

The benchmarks are the wider net, and both agree exactly:

| | phases | checksums |
|---|---:|---|
| `CratonBench` (`-XX:+UseG1GC -Xmx8g`) | 7 | **7/7 identical** |
| `CratonBenchC2` (`-Xmx4g`) | 3 | **3/3 identical** |

`arithmetic 5000000003999999995`, `fib 701408733`, `sieve 9592`,
`matrix 173943680`, `hashmap 1549999915000000`, `stringregex 5000050000`,
`bintrees 68332206` — bit-identical with the flag on and off, plus 2 395 unit
tests green.

**No performance claim is made from those two runs**, and one of them is worth
recording as a warning rather than a result. A single uncontrolled
`CratonBenchC2` pair read `6103 ms` off against `3108 ms` on — an apparent 2x.
Interleaved ten rounds per arm with a control arm, it is nothing:

```text
A (flag off) median 3211 ms   C (control) median 3326 ms
B (flag on)  median 3204 ms
noise floor 3.6%   effect -2.0%   VERDICT: UNMEASURABLE
```

The first pair was noise on a shared box, and it was noise in the flattering
direction. That is the failure mode the control arm is for, and a 2x that
evaporates under interleaving is a reminder that single-run benchmark pairs on
this host are not evidence of anything.
## 6c. The pressure gate: closed, and the answer is NO

§6a left one item: every probe above is under 1.3 KB with slack in the register
file, and the flag's risk is specific — `ir_gp_file()` is five deep, and
`ir_reserve_carried_enabled`'s pass runs **last**, over
`free = ir_gp_file() - taken`. The crossblock arm spends that file earlier, so on
a method whose live set already exceeds five it could take registers from values
read every iteration and give them to values read once.

`probes/RegPressure.java` is built for exactly that and nothing else: six
loop-carried `long` accumulators against a five-register file, plus three
cross-block single-use values (`p`, `q`, `r`) defined at the top of the body and
each consumed in one arm of the branch below. `mixNarrow` is the same loop with
three accumulators — tight, but not over-subscribed.

**The worry is real, and the flag loses.** Both variants, both above their floors:

```text
mix        (6 accumulators)  A 215.0 | C 217.0 | B 243.0 ms
                             floor 0.9%   effect +12.5%   ratio 1.125x   SLOWER
mix        (confirmation)    A 207.0 | C 196.0 | B 221.0 ms
                             floor 5.5%   effect  +9.7%   ratio 1.097x   SLOWER
mixNarrow  (3 accumulators)  A 130.0 | C 127.0 | B 135.5 ms
                             floor 2.3%   effect  +5.4%   ratio 1.054x   SLOWER
```

The census names the mechanism. On `mix` (`peak_live=35`, `splits=63`):

| | `resident` | `single_use` | `split_or_spilled` | `carried_reserved` | body |
|---|---:|---:|---:|---:|---:|
| flag off | 5 | 57 | **12** | **5** | 3164 |
| flag on | **6** | 32 | **28** | **2** | 3218 |

Twenty-five more values admitted buys **one** more resident. What it actually
does is more than double the values the scan splits or spills (12 → 28) and take
three registers off the loop-carried reservation (5 → 2) — and `mix` then emits
three more stores and six more loads than the arm that admitted nothing.

This is the shape §5's cost model does not price, and the reason is that §5's
benefit term assumes the register is **free to take**. Under pressure it is not:
its real price is whatever the value that would otherwise have held it was worth,
and on this probe that value is read every iteration. `static_uses >= 2` cannot
see that either, so the successor rule §6 proposes needs a third term — an
occupancy check — and not just better-priced first two.

**That term was then built — §6d.** It eliminates this section's 12.5% case
outright (with it on, `mix` compiles to the same bytes as the arm that admits
nothing) and keeps §3's win. It does **not** close the lane: `mixNarrow` still
loses 6.7%, and §6d shows why no rule of that shape can fix it.

**`CRATONVM_JIT_IR_RESIDENCY_CROSSBLOCK` therefore stays default OFF, now for a
measured reason rather than for want of evidence.** The mechanism is sound and
the loop win in §3 is real, but it is not safe to hand out a shared five-register
file on a rule that never asks whether anything else needed it. §6d makes it ask,
which removes this section's worst case and still leaves the flag OFF.

### The measurement error that nearly hid this, and how to not repeat it

The first run of this probe reported **1.000x / UNMEASURABLE over a 0.5% floor**
and would have closed this gate the wrong way.

Both arms were executing **identical machine code**. At `probe.reps=400
probe.n=20000`, `RegPressure.mix` is reached through the **OSR** door, and
`CRATONVM_DBG=jitc` says what that door does with the optimizing pipeline:

```text
osr ir-eligibility: ACCEPTED for RegPressure.mix(I)J -- INERT at this door,
                    which is single-pass only
[ir] admission RegPressure.mix(I)J: admitted to the optimizing pipeline
```

The IR pipeline **runs** — which is why `CRATONVM_DBG_IR_LINEAR_SCAN=1` printed a
census that differed between the arms, `resident` 5 → 6 and `carried_reserved`
5 → 2, exactly the starvation this section is about. The published body is
`osr/sp len=3354` in both arms regardless. **A differing linear-scan census is
not evidence that differing code ran.**

What fixes it is the invocation-count door: `c2_threshold` is 20 000
*invocations*, so the probe must be run as many calls with a short loop
(`reps=300000 n=40`) rather than few calls with a long one. Then
`full/ir RegPressure.mix(I)J` is published, at 3164 vs 3218 bytes, and the
regression appears immediately.

Two checks worth making standard before trusting any `flag-ab.sh` verdict on a
new probe, both cheap:

* `CRATONVM_DBG=jit-disasm | grep full/ir` — **is there an optimizing body at
  the settings the A/B uses?** `osr/sp` and `full/sp` are single-pass; a flag in
  `ir_lower.rs` cannot move them.
* **The two arms' `len=` must differ.** Identical bodies mean the A/B is
  measuring the noise floor twice, and it will faithfully report
  `UNMEASURABLE` — which reads exactly like "the flag is safe here".

`FieldLoop`, `FibCall` and `ManyExits` were checked against both and are
unaffected: each publishes a `full/ir` body at its measured settings, and each
publishes a *different* one per arm.
## 6d. The occupancy term, built twice — necessary, and not sufficient

§6c ends by saying the successor rule needs an **occupancy term**: the flag may
not take a register without asking what else wanted it.
`CRATONVM_JIT_IR_RESIDENCY_CROSSBLOCK_BUDGET` (**default ON**, opt out with `=0`)
is that term. It was built in two forms, and the second one is where the useful
result is.

### First form: a count. Too blunt, and it removed the win.

```text
budget = ir_gp_file().len() - |carried candidates|
```

Reserve what `ir_reserve_carried_enabled`'s pass will want, let the crossblock arm
have the remainder. It does eliminate the `RegPressure` regression — and it
eliminates the transform. `FieldLoop.sum` has **six** carried candidates against a
five-register file, so the budget is zero there too and the body reverts to its
slow 1052 bytes. Most loops have five or more carried candidates; a count makes
the flag inert in exactly the place it works.

### Second form: a displacement price, and it separates the two probes

The carried pass serves its candidates in descending loop-weighted use count and
stops when the file runs out, so the value a crossblock admission actually
displaces is the **weakest one that would still have been served** —
`carried_w[file_len - 1]` — and each further admission displaces the next one up.
The bar rises as the file empties, which stops a run of cheap single-use values
from evicting the whole carried set one register at a time.

The comparison is `>=`, not `>`, and that is measured rather than chosen. On
`FieldLoop` the carried weights are `[20, 11, 10, 10, 10, 10]`, so the price is
10 — and the induction variable the whole transform exists to serve weighs
exactly 10. Under `>` all ten candidates are refused and the win is gone.

It works, on the bodies:

| | crossblock OFF | crossblock, no budget | crossblock + budget |
|---|---:|---:|---:|
| `FieldLoop.sum` | 1052 | **1039** (the win) | **1039** — kept |
| `RegPressure.mix` | 3164 | 3218 (**+12.5% slower**) | **3164** — byte-identical to OFF |

The 12.5% regression is not merely reduced, it is **structurally impossible**:
with the budget on, `mix` compiles to the same bytes as the arm that admits
nothing. `carried_w` there is `[60, 21, 21, 21, 21, ...]`, so the price is 21
against candidates weighing 10, and every one is refused.

### And it is not sufficient. `mixNarrow` still loses.

```text
mixNarrow, budget ON   A 179.0 | C 179.0 | B 191.0 ms
                       floor 0.0%   effect +6.7%   ratio 1.067x   SLOWER
```

**This is the interesting part of the section, because it is not a tuning
failure.** `mixNarrow` and `FieldLoop.sum` are indistinguishable to any rule of
this shape:

| | carried weights | price | candidate weight | admitted | outcome |
|---|---|---:|---:|---:|---|
| `FieldLoop.sum` | `[20, 11, 10, 10, 10, 10]` | **10** | **10** | 3 | **0.924x — faster** |
| `RegPressure.mixNarrow` | `[50, 21, 21, 11, 10, ...]` | **10** | **10** | 3 | **1.067x — slower** |

Same price, same candidate weight, same number admitted, opposite sign. **Loop
weight is provably not the discriminating variable**, and no a-priori scoring
function over `live.weight` can separate these two probes, because on the inputs
such a function reads they are the same probe.

What differs is not any value's worth but the *total* contention around it —
`FieldLoop.sum` carries one accumulator through a branchless body,
`mixNarrow` carries three through a branch — and the effect of that shows up in
the allocator's own output (`splits`, `scan_spills`) rather than in any weight.

So the honest shape of the remaining work is a **feedback** rule, not a better
score: admit, re-run or re-inspect the allocation, and keep the admission only if
the result did not get worse. `plan_register_residency` today reads
`alloc.segments` once and decides; asking the question the other way round is a
larger change than this page should make on the strength of two probes.

### Status

* `CRATONVM_JIT_IR_RESIDENCY_CROSSBLOCK` — **still default OFF.** §6c's losing
  shape is narrowed, not closed.
* `CRATONVM_JIT_IR_RESIDENCY_CROSSBLOCK_BUDGET` — **default ON.** It is a strict
  improvement wherever the outer flag is enabled: it keeps the `FieldLoop` win,
  makes `RegPressure.mix` byte-identical to not running at all, and is inert on
  `FibCall.fib` and `ManyExits.pick` (no loop ⇒ fewer carried candidates than
  registers ⇒ price 0 ⇒ nothing to outbid). Turning it off restores the
  unbudgeted behaviour for anyone re-measuring §6c.
## 7. What this says about the `fib` lane

§4 is a negative result on `fib` and it belongs with the budget page's other
two: the precise-maps mirror bought nothing, the equal-depth sink was 2.4%
slower, and admitting six values to registers instead of two is free. Four
hypotheses measured on that method now, and the only movement came from a
*different* method's loop.

That is consistent with the budget page's conclusion rather than a challenge to
it: `fib` is 3.7x because a CratonVM call is ~36 instructions to HotSpot's ~10,
and every term in that sum is small. §3's win is the shape of what does work —
it did not remove a term from the per-call contract, it removed a memory
round-trip from something executed twenty thousand times inside one call.

The one genuinely encouraging number for the per-call budget is still in §4's
table: **three home stores disappeared.** `value_home_droppable` /
`op_home_is_one_store_rax` already exist and already fire, gated on
`deopt_nameable[id]`, which requires `holders[reg] == 1` — exclusive register
ownership across the whole method. Widening what can satisfy that is the lane
that attacks the per-frame contract directly.

## 8. Reproducing

```bash
javac -d probes/out probes/FibCall.java probes/FieldLoop.java
EXE=./target/release/cratonvm

# the census, and the optimizing body (NOT the osr/sp one at these settings)
CRATONVM_DBG_IR_LINEAR_SCAN=1 $EXE -cp probes/out -Dprobe.n=26 -Dprobe.reps=1 FibCall
CRATONVM_DBG=jit-disasm CRATONVM_DBG_JIT_DISASM=FieldLoop.sum \
  $EXE -cp probes/out -Dprobe.reps=3000 -Dprobe.n=20000 FieldLoop | grep full/ir

bash tools/tier-ab/flag-ab.sh -Exe $EXE -Cp probes/out -Class FieldLoop \
  -Flag CRATONVM_JIT_IR_RESIDENCY_CROSSBLOCK -On 1 -Off 0 \
  -D probe.reps=3000 -D probe.n=20000 -Rounds 13

# the falsification probe of section 5 -- loop-free, SIX epilogues
javac -d probes/out probes/ManyExits.java
bash tools/tier-ab/flag-ab.sh -Exe $EXE -Cp probes/out -Class ManyExits -Flag CRATONVM_JIT_IR_RESIDENCY_CROSSBLOCK -On 1 -Off 0 -D probe.reps=120 -D probe.n=200000 -Rounds 21

# correctness, section 6b
bash tools/jit-flag-soak.sh $EXE vm/tests/resources:cratonvm CRATONVM_JIT_IR_RESIDENCY_CROSSBLOCK=1
$EXE -Xmx8g -XX:+UseG1GC -cp bench-classes CratonBench    # compare checksums

# section 6c -- the pressure gate. NOTE the settings: many INVOCATIONS with a
# short loop, so `mix` reaches the invocation-count door. With reps=400 n=20000
# it arrives through OSR instead, which is single-pass only, and both arms then
# execute byte-identical code while the linear-scan census still differs.
javac -d probes/out probes/RegPressure.java
CRATONVM_DBG=jit-disasm $EXE -cp probes/out -Dprobe.reps=300000 -Dprobe.n=40 RegPressure | grep full/ir
bash tools/tier-ab/flag-ab.sh -Exe $EXE -Cp probes/out -Class RegPressure -Flag CRATONVM_JIT_IR_RESIDENCY_CROSSBLOCK -On 1 -Off 0 -D probe.reps=1200000 -D probe.n=40 -Rounds 15
```
