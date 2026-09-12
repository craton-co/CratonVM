# The optimizing tier's loop body is mostly code it never runs

**Status:** one default flipped, one wrong answer fixed, two switches left OFF
because they measured as nothing. §7 adds the `sumWide` arm §6 left open; §8
answers why its four hoisted reads stayed four and gives the tier the
redundant-load elimination it turned out not to have; §9 takes §6's other open
item; §10 closes the rest of the list, two of them with reasons rather than
work; §11 takes three of §10's own follow-ups, and corrects §8's reading of
its own zero.
**Shape:** `probes/FieldLoop.java` `sum` — `for (i…) a += this.fx;`
**Predecessor:** `c2-the-phi-copy-staging-register-20260911.md`, whose §4 asked
the question this answers.

---

## 1. What was being asked

§4 of the phi-copy document priced the residual tiering inversion as a
per-iteration instruction budget and left three follow-ups:

| | single-pass | optimizing |
|---|---:|---:|
| instructions per iteration | 20 | 26 |
| of which TAKEN branches | 1 | **4** |
| frame loads + stores | 4 | 4 |

* run LICM before the unroller, so the unroller reaches this shape;
* chase the `CRATONVM_JIT_IR_THIS_NONNULL` anomaly — deleting two instructions
  made this loop **20% slower**, which `docs/JIT_OPTIMIZATION.md` calls "the
  most promising lead for the residual inversion";
* explain the taken-branch row, which is a bigger relative gap than the
  instruction count and which §9's branch-polarity switch did not move.

All three turned out to be the same question, and the disassembly answers it.

## 2. The loop, as bytes

`full/ir FieldLoop.sum(I)I`, compiled through the invocation-count door
(`-Dprobe.reps=5000 -Dprobe.n=12`, which is what makes the tier compile a body
rather than only an OSR entry). Header at `0x1cb`, back edge at `0x362`.

```asm
1cb: cmp dword [rel <epoch>],0     ; layout epoch guard
1d5: jne 20d                       ; not taken
1db: mov rax,[rbp-58h]             ; `this`
1df: test rax,rax                  ; the receiver null check
1e2: je  20d                       ; not taken
1e8: test byte [rax+0Fh],4         ; GC_FLAG_COMPACT
1ef: je  201                       ; not taken
1f5: movsxd rax,[rax+10h]          ; ← the read. The only instruction that is the program
1fc: jmp 23e                       ; ***TAKEN*** over the legacy arm and the helper call
     ┌─ 201..20c   legacy 16-byte-cell read      (12 bytes, never executed)
     └─ 20d..23d   the checked helper call       (49 bytes, never executed)
23e: mov r13,rax                   ; …accumulate…
…
25d: cmp ebx,r14d
260: jge 367                       ; the loop test — not taken
266: test byte [rel <poll flag>],0FFh
26d: je  358                       ; ***TAKEN*** over the safepoint slow path
     └─ 273..357   safepoint slow path          (229 bytes, never executed)
358: mov r12,r15                   ; phi edge copies
35b: mov rbx,[rbp-98h]
362: jmp 1cb                       ; ***TAKEN*** back edge
```

**The loop spans 412 bytes. About 122 of them ever execute.** Two of its three
taken branches exist for no reason except that cold code was emitted inline,
between the hot instruction that precedes it and the hot instruction that
follows.

That is the taken-branch row, and it is also the answer to the `THIS_NONNULL`
anomaly. A hot path threaded through 412 bytes in four fragments is pinned to
its byte offsets: the fragments land where the cold blocks leave them, relative
to every 16-, 32- and 64-byte boundary the front end cares about. Removing the
`test rax,rax` / `je` pair deletes **nine bytes** from the middle of that
arrangement and moves everything after it. "Deleting two instructions cannot
slow a loop by 20% on its own" is right, and this is the mechanism it was
looking for: the two instructions are not the cost, they are the *shim*.

## 3. What LICM was doing, which was nothing

`CRATONVM_DBG_LICM=1` on this exact loop:

```
[DBG_LICM] header 5: body 9 node(s), 1 load(s), hard_barrier=false writes_memory=false
[DBG_LICM] load 18 (inputs [17, 9, 3, 8]): HOIST base 3 addr 8
[DBG_LICM] hoisted 1 invariant load(s) to loop pre-header(s)
```

LICM reports that it hoisted `this.fx` out of the loop. §2 above is the code it
emitted afterwards, and the read is still in the body — with its epoch guard,
its null check, its compact test and its jump. Nine of the ~24 hot instructions,
per iteration, for a field nothing writes.

The hoist moved the load's **control** edge to the pre-header and left its
**memory** edge naming the loop header's memory phi. `ir_schedule::find_best_block`
places a data node in the *deepest* block dominated by all of its input blocks —
so the memory phi's block, the header, wins, and the load is scheduled straight
back into the loop it was just hoisted out of. A hoist nothing observes is not a
hoist.

The general hoist arm's own comment describes the fix it never performed:

> The memory token to use at the pre-header: the region's *entry* memory phi
> input if a memory phi exists, else any invariant memory.

The read-hoist arm next to it — the restricted one that only fires for a loop
"that only its own reads and guards disqualify" — has always moved both edges,
with a `loop_entry_memory` helper that was already written. `FieldLoop.sum` has
no disqualifying barrier at all, so it never went near that arm.

## 3a. And what it was doing wrong: a NullPointerException out of nowhere

Making the hoist real exposed a second thing, which turned out not to need the
switch at all.

A hoist to the pre-header is **speculative**. The pre-header runs on every
entry; the body does not. A load that reaches the pre-header therefore executes
for a loop that iterates zero times — and a `getfield` carries its own null
check, so speculating the load speculates the `NullPointerException` with it.

`probes/ZeroTripHoist.java`:

```java
static int walk(N o, int n) {
    int a = 0;
    for (int i = 0; i < n; i++) { a += o.v; }
    return a;
}
```

`walk(null, 0)` never dereferences `o`, so it returns 0. Temurin 25 returns 0.
CratonVM's optimizing tier **threw**:

| configuration | `walk(null, 0)` |
|---|---|
| HotSpot (Temurin 25.0.3) | `0` |
| `CRATONVM_JIT_FORCE_C2=1` | **NullPointerException** |
| `CRATONVM_JIT_FORCE_C2=1 CRATONVM_JIT_NO_LICM_READ_HOIST=1` | **NullPointerException** |
| `CRATONVM_JIT_FORCE_C2=1 CRATONVM_JIT_LICM=0` | `0` |
| `CRATONVM_C2_SUPERSEDE=0` (single-pass) | **NullPointerException** |

It is LICM, it is the general hoist arm (it survives switching the read-hoist
arm off), and it is **not** the memory-edge switch — it reproduces with that
switch off, because the control-edge hoist alone was enough once the rest of the
pipeline agreed to honour it.

So the hoist now asks permission before it speculates, unconditionally and not
behind any flag. Three answers are accepted and nothing else:

* the load is anchored at the **header** itself — the header runs whenever the
  loop is reached, zero trips included, so the pre-header is not earlier in any
  execution that matters;
* the base is the **receiver**, which the JVM guarantees non-null at the call
  site and SSA gives no other definition — the fact `ir_check_elim` already
  seeds;
* `ir_check_elim::definitely_non_null` already answers for the base.

`FieldLoop.sum` reads `this.fx`, so it takes the second answer and keeps every
byte of §5's result. `ZeroTripHoist.walk` is static, its first parameter can be
null, and it is refused.

The test has two arms on purpose. A guard that refused *everything* would pass a
one-armed "no NPE" test while deleting the optimization, so
`an_invariant_load_of_a_maybe_null_base_is_not_hoisted_out_of_a_maybe_empty_loop`
runs the same graph twice and differs only in whether the base is the receiver:
refused for the static parameter, hoisted for the receiver.

The `CRATONVM_C2_SUPERSEDE=0` row in that table invited the conclusion that the
single-pass tier has the same bug in its own LICM (`jit/src/x64/licm.rs`). It
does not, or at least this is no evidence of it: **after the fix, that row
returns 0 as well**, from a change confined to `ir_optimize::licm`. So
`CRATONVM_C2_SUPERSEDE=0` does not keep the IR path away from this method — it
stops the optimizing body from superseding, not from being compiled — and the
throw was the same hoist in all four rows. Worth knowing for the next
bisection, because that switch reads like a tier selector and is not one.

| configuration | before | after |
|---|---|---|
| `CRATONVM_JIT_FORCE_C2=1` | NPE | `0` |
| `CRATONVM_JIT_FORCE_C2=1 CRATONVM_JIT_IR_LICM_MEM_EDGE=1` | NPE | `0` |
| `CRATONVM_C2_SUPERSEDE=0` | NPE | `0` |
| default | `0` | `0` |

## 4. What landed

Three switches plus one unswitched correctness fix. Every switch reads its flag
live rather than caching it in a `OnceLock`, so an in-process A/B can see both
arms — the trap `ir_per_copy_frames_enabled` documents.

| flag | default | what it does |
|---|---|---|
| `CRATONVM_JIT_IR_LICM_MEM_EDGE` | **ON** | moves a hoisted load's memory edge to the loop-entry state, which is what actually gets it scheduled outside the loop |
| `CRATONVM_JIT_IR_LICM_BEFORE_UNROLL` | OFF | runs LICM before the unroller instead of after it |
| `CRATONVM_JIT_IR_POLL_OUTLINE` | OFF | emits a safepoint poll's slow path after the body, and inverts the poll's test so the fast path falls through |
| *(unswitched)* | — | LICM refuses to hoist a load it cannot prove safe to speculate (§3a) |

The memory edge is ON because it cleared the bar this tree uses for a default,
which is the one the `IR_SINK_LATE` and `HOT_LAYOUT` flips cite: the effect is
outside the noise floor with a same-config control (§5), CratonBench's seven
checksums are byte-identical in every arm, and the whole `cratonvm-jit` suite —
2,373 unit tests and the 145 `ir_vs_singlepass` differential cases — passes
gate-ON exactly as gate-OFF. `CRATONVM_JIT_IR_LICM_MEM_EDGE=0` is the kill
switch.

The other two are OFF because they measured as nothing, which is a reason to
keep a switch rather than to ship it.

### Pass order, and why it is not the lever

The premise in the predecessor document was that the partial unroller could not
reach `FieldLoop.sum` because the invariant read is pinned to the header
(`escapes_or_pinned`). **That premise is wrong, and the check is easy: with
`CRATONVM_JIT_IR_PARTIAL_UNROLL=1` the method compiles to 2385 bytes instead of
1054.** It unrolls today.

Moving LICM ahead of the unroller changes nothing on its own either, and for a
reason worth writing down: the unroller builds its clone set by DATA dependence,
so a read the accumulator consumes is cloned per copy whatever its control
anchor says. The order only starts to matter once the memory edge moves too —
then the copies are identical expressions and the trailing GVN folds them to
one. `licm_before_unroll_with_the_memory_edge_shares_one_read_across_copies`
pins exactly that: two reads become one, and it takes both switches.

### The outlined poll

The poll's fast path was the taken branch. `JZ` skipped the slow block, so every
iteration of every loop in this tier branched forward over ~230 bytes it never
entered. Outlining inverts it to `JNZ` and emits the block after the body, with
a `JMP` back to the instruction after the poll.

It is deferrable because the slow path's only per-site input is
`spill_high_water` — `emit_safepoint_map_if_enabled` reads nothing else — and
the oop map it records is keyed by the CALL's return address, which is correct
wherever that call ends up.

The test that matters is not the return value. Getting the inversion backwards
gives a loop that calls into the runtime every iteration and *still returns the
right answer*, so
`an_outlined_safepoint_poll_stops_when_the_inline_one_does_and_not_otherwise`
makes the poll flag the axis: set, the slow path must run the same number of
times in both arms (so the block is reached and the return jump lands); clear,
it must not run at all (so the polarity is right).

## 5. Measurements

`probes/FieldLoop.java`, `-Dprobe.reps=20000 -Dprobe.n=20000`, nine rounds,
arms interleaved with the order flipped by round, medians, no samples discarded.
Every flag A/B carries `flag-ab.sh`'s same-config **control** arm, and its
spread is the noise floor quoted beside each effect — an effect inside the floor
is reported as UNMEASURABLE rather than as a number. The checksum was
`1200150000` on all 216 runs.

### The switches

| # | switch | ratio | floor | verdict |
|---|---|---:|---:|---|
| 1 | `IR_POLL_OUTLINE` | 1.013x | 1.7% | UNMEASURABLE |
| 2 | `IR_LICM_MEM_EDGE` | **0.589x** | 8.2% | **faster** |
| 3 | `IR_THIS_NONNULL` | 1.023x | 4.6% | UNMEASURABLE |
| 4 | `IR_LICM_BEFORE_UNROLL` (with 2 and the partial unroller on) | 1.068x | 2.6% | **slower** |

### The tiers

Same probe, `tier-ab.sh`: A is the single-pass tier (`CRATONVM_C2_SUPERSEDE=0`),
B the optimizing tier (`CRATONVM_JIT_FORCE_C2=1`), C a second A.

| memory edge | baseline | optimizing | ratio | floor | verdict |
|---|---:|---:|---:|---:|---|
| OFF | 475 ms | 526 ms | **1.120x** | 6.4% | optimizing SLOWER — the inversion |
| ON | 475 ms | 316 ms | **0.680x** | 4.1% | optimizing FASTER |

**The inversion on this shape is closed and reversed**: the optimizing tier goes
from 12% slower than the tier it supersedes to 32% faster, from one edge in the
graph.

### Reading the rows

* **Row 2 is the whole result.** Nine of the loop's ~24 hot instructions were a
  re-read of a field nothing writes; the loop is ~12 instructions now, and
  0.589x is close to the 12/24 the instruction count predicts.
* **Row 1 is a negative result worth keeping.** Outlining the poll removes 229
  of the loop's 412 bytes and one of its three taken branches per iteration, and
  it is worth 1.3% against a 1.7% floor. A correctly predicted branch over cold
  bytes costs approximately nothing, because instruction fetch follows the
  predicted target and never reads the bytes being skipped. §2's byte anatomy is
  a true description of the code and a bad model of its speed.
* **Row 3 retires a lead.** `docs/JIT_OPTIMIZATION.md` recorded this switch as
  ~20% SLOWER and called it "the most promising lead for the residual
  inversion". It is 1.023x inside a 4.6% floor. That document has been
  corrected.
* **Row 4 is why a switch stays OFF rather than being deleted.** Reordering the
  two loop passes is 6.8% slower in the one combination where it does anything
  at all, which is a finding, not a wash.

### Apparatus, because it cost one wrong reading

An earlier tier A/B reported the baseline arm at 966 ms against 514 ms in the
run before it, which is not a thing a flag can do to the arm that ignores it.
The editor's language server runs `cargo check` on every Rust edit, and a single
active `rustc` takes this host's floor from about 4% to **21.4%** — larger than
two of the three effects above. The control arm is what caught it: a
configuration disagreeing with itself by more than the effect being claimed is
the signal to stop and clean the host, not to report the number.


## 6. What is still open

* **The byte-span theory is dead, and §2 should be read as anatomy rather than
  as a cost model.** Outlining the largest cold block and one taken branch per
  iteration measured as nothing. A correctly predicted branch over cold bytes
  costs about nothing, because instruction fetch follows the predicted target
  rather than the linear address — so "412 bytes spanned, 122 executed" is a
  true and vivid description of the code and not an explanation of its speed.
  The thing that moved this loop was deleting work.
* **The `getfield` cold arms are still inline**, and on the evidence above that
  is fine. 61 of the 412 bytes and the third taken branch; outlining them is a
  bigger change than the poll was, and the poll bought nothing.
* **`CRATONVM_JIT_IR_THIS_NONNULL`'s 20% anomaly does not reproduce** (§5).
  `docs/JIT_OPTIMIZATION.md` has been corrected: it was the best lead anyone had
  on the residual inversion, and there is no longer an effect for it to be a
  lead on.
* ~~`sumWide` is unmeasured.~~ **Measured — see §7.** It helps more there, not
  less, and the reason is not the one this bullet guessed.
* ~~**The speculation permission is narrow on purpose**~~ (§3a). **Widened once
  — see §9.** A loop whose bound is provably positive now hoists, because a
  body that always runs makes the pre-header not speculation at all. "A base
  proven non-null by a dominating check" is still refused, and still open.
* **Nothing aligns a loop header**, and there is no nop/pad emitter in this
  backend at all. Worth less than it looked like before §5, for the same reason
  as the first bullet.

---

## 7. `sumWide`, which §6 said should be checked rather than assumed

`probes/FieldLoop.java`'s other arm: the same loop with four INDEPENDENT
accumulators, all reading the same field. The probe's own comment says it is not
a second benchmark — the ratio between the two arms separates a latency
bottleneck (which independent work overlaps) from extra work (which does not).

Same apparatus as §5, `-Dprobe.wide=true`, nine rounds, checksum `4800600000`
on all 108 runs.

| | baseline | optimizing | ratio | floor |
|---|---:|---:|---:|---:|
| memory edge OFF | 1480 ms | 1418 ms | 0.964x | 1.2% |
| memory edge ON | 1454 ms | **649 ms** | **0.445x** | 0.9% |

The flag against itself on the optimizing tier: 1436 ms → 674 ms, **0.472x**,
floor 1.3% — a *larger* win than `sum`'s 0.589x.

### Two things this corrects

**§6 guessed the mechanism wrong, and so did the guess that replaced it.** The
bullet said "four reads become one". Reading the bytecode — four `getfield #7`
sites at bci 21, 28, 36 and 44, same field, same receiver — the obvious
correction was that GVN and the receiver-guard CSE would have collapsed them to
one load long before LICM looked, making the win *smaller*. Both were wrong, and
the disassembly says so plainly:

| `sumWide` loop, header to back edge | instructions | field-read sequences inside |
|---|---:|---:|
| memory edge OFF | 145 | **4** |
| memory edge ON | **69** | **0** |

Nothing collapsed them. Each site kept its own epoch guard, null test, compact
test, read and jump, and all four ran every iteration. The accurate sentence is
*four read sequences become zero inside the loop*, and it is worth more than one
because there were four of them to remove.

**There was no inversion on this shape to begin with.** With the memory edge off
the optimizing tier is already 0.964x here — 3.6% against a 1.2% floor — where
`sum` is 1.120x. So the tiering inversion is narrower than "the optimizing tier
is slower": it is a property of a loop whose *entire* body is one guarded read,
where the per-iteration overhead has nothing to hide behind. Widen the body with
independent work and it disappears on its own.

### What this opens

The four reads are hoisted, but they are still **four**: the pre-header holds
four complete read sequences where one would do. GVN does not dedup them even
once they share a control anchor and a memory state, which it should be able to
after the memory edge moves. (**It should not — see §8.** Two reasons, and the
sentence above names neither.) Once per call rather than once per iteration, so it
is worth little on a 20,000-iteration loop and proportionally more the shorter
the loop — and "identical loads in the same block do not GVN" is a fact about
the pass worth knowing whatever it is worth here.

---

## 8. Why the four hoisted reads stayed four

§7 ended on the one thing it could not explain: the memory-edge hoist moves
all four of `sumWide`'s reads to the pre-header, and **they stay four**. The
note guessed that GVN "does not dedup them even once they share a control
anchor and a memory state, which it should be able to".

It should not, and the reason took one grep.

### Two reasons, and the second is the interesting one

`gvn` skips the node entirely:

```rust
if node.op == Op::Dead || !node.op.is_pure() { continue; }
```

`Op::Load` is not in `Op::is_pure()`'s list and cannot be: a load's value
depends on the heap, which its `(op, ty, inputs)` key does not describe. So
there was no redundant-load elimination in the IR tier at all — not a weak one,
none.

Teaching `gvn` about loads would not have fixed it either, and this is the part
worth keeping. The builder advances the memory token on every **read**:

```rust
// ir.rs, getfield
self.mem = load;
```

That edge exists for a real reason — a later store to a possibly-aliasing cell
must be ordered after this read — but it means four `getfield`s in a row form a
CHAIN: `L1(mem=M)`, `L2(mem=L1)`, `L3(mem=L2)`, `L4(mem=L3)`. No two of them
share a memory input, so input-equality rejects every pair. The four reads were
never going to merge on identity, in the pre-header or anywhere else.

### What landed

`eliminate_redundant_loads` keys on everything EXCEPT the memory token, and
then asks the question the token was hiding: is `B`'s memory state reachable
backwards from `A`'s through nodes that cannot have written the cell `B` reads?
Only `MemAccess::{FieldRead, ArrayRead, LengthRead}` may appear on that path —
reads, which advance the token and write nothing. One store, one call, one
allocation, one memory φ, and the walk stops.

Four conditions, each with a test that fails if it is dropped:

| | what it means | the test |
|---|---|---|
| same address | every input but the token is node-identical, and an `Op::Load`'s field index IS an input | `four_reads_..._become_one` |
| same heap | the chain walk; `from == to` is the zero-length case | `a_write_between_two_reads_..._stops_the_merge` |
| same place | `A`, `B` and every node between share a control anchor | `two_reads_on_opposite_arms_of_a_branch_are_not_merged` |
| same program point | `frame_snapshot` is in the key, as it is in `gvn`'s | (shared with `gvn`) |

The third is not a formality. Without it, `c != 0 ? o.v : o.v` merges two reads
whose memory inputs are already equal, pulling a read and its null check onto a
path that need not take it — the identical bug §3a records LICM having shipped.

### The chain is repaired, not short-circuited

`B` sits in the memory chain, so `replace_all_uses(B, A)` alone is wrong in the
quiet direction. A store that named `B` as its token would be re-pointed at
`A`, and lose its ordering against every read BETWEEN them — reads that are
still there. The two kinds of edge are separated: a *token* user of `B` is
spliced to `B`'s own incoming token, and only then do the remaining *value*
users and the safepoint slots follow the value to `A`.

`removing_a_read_splices_the_memory_chain_instead_of_shortcutting_it` is the
test, and it is built so that the survivor and the correct token are different
nodes — otherwise the wrong implementation passes it.

### The emitted loop

Same apparatus as §5 and §7. `sumWide`'s loop, header to back edge, in the
2x2 against the memory-edge hoist:

| | loop instructions | field reads inside | body bytes |
|---|---:|---:|---:|
| mem-edge ON, no CSE — today's default | 69 | 0 | 1869 |
| mem-edge ON + CSE | 70 | 0 | **1224** |
| mem-edge OFF, no CSE | **145** | **4** | 1890 |
| mem-edge OFF + CSE | **91** | **1** | 1224 |

Rows 1 and 3 are §7's numbers, reproduced by a different binary a day later,
which is what says the apparatus is measuring the thing it claims to.

### The clock

Nine rounds, interleaved, medians, a same-config control whose spread is the
floor. Checksums collapse to exactly three values across every run — one per
probe shape — so no arm computed anything different.

| | probe | ratio | floor | verdict |
|---|---|---:|---:|---|
| L1 | `sumWide`, mem-edge ON | 1.010x | 2.5% | UNMEASURABLE |
| L2 | `sumWide`, mem-edge OFF | **0.627x** | 1.4% | **faster** |
| L3 | `sum` (one read) | 1.013x | 3.3% | UNMEASURABLE |
| L4 | tier A/B, `sumWide`, CSE ON + mem-edge OFF | **0.598x** | 2.4% | optimizing faster |

**The predictions were registered before the binary existed**, and three of four
held. L1, L3: nothing measurable, because with the memory edge on, the four
reads are already out of the loop and collapsing them saves three read sequences
ONCE PER CALL on a 20,000-iteration loop. L2: the large win, because with the
memory edge off the reads are still IN the loop and the pass deletes three of
four.

The one that was wrong is worth keeping. The prediction said L2 would land "near
the 0.445x that mem-edge ON reaches". It is 0.627x, and the disassembly had
already said why before the clock ran: this pass collapses four reads to one but
leaves that one INSIDE the loop (91 instructions), where the memory edge hoists
it out entirely (69). The two overlap, and on this shape the memory edge is
strictly the better of the pair.

L4 is the independent result. §7 measured mem-edge OFF at 0.964x — no inversion
on `sumWide`, but no win either. With load-CSE instead, the same configuration
is **0.598x**. Two mechanisms with nothing in common — delete three reads, or
move one — reach nearly the same place.

### And it fires on nothing in CratonBench

The census, on the seven-benchmark suite, with the switch on: **0**. Not a
refusal — `CRATONVM_DBG_LOAD_CSE=1` prints a line per refused graph and there
are none, and the same flag on `FieldLoop` prints `read 23 is redundant with
21`, `25`, `27`, three compiles, matching the census of 9. The pass runs on
those graphs and finds nothing: no method in `arith`, `fib`, `sieve`, `matrix`,
`hashmap`, `stringregex` or `bintrees` reads one cell twice out of one heap
state in one block.

**So the switch stays OFF by default**, and that zero is the reason rather than
the 1.010x. A pass that runs on every compile and fires on nothing in the
measured workload has not earned a default, however good it looks on the probe
written for it. It is there for the shape LICM cannot reach — a loop with a
barrier in it, or straight-line redundancy — and when a workload with that
shape turns up, the census is what will say so.

---

## 9. The other open item: a permission that refuses loops it does not have to

§6's second bullet: the speculation permission §3a installed is *header-anchored,
receiver, or `definitely_non_null`*, and "a loop with a provably positive bound
... is hoistable and is refused today. Widening it is an optimization; each
widening is a new claim about when the body must run."

There is exactly one such claim this tree can already prove, and it was sitting
in the unroller.

### The claim

A pre-header hoist is speculative for ONE reason: the pre-header runs when the
body does not. If the body always runs, every fault the hoisted read can raise
was going to be raised by the first iteration anyway, and the base needs no
vouching at all. `analyze_counted_loop` — the unroller's own analysis — answers
this for a single-back-edge loop, and two of its four answers mean yes:

* `Ok(c)` with `c.trip >= 1`: a constant trip count that is not zero.
* `Err(NotCounted::TripOverCap)`: **the interesting one.** The unroller returns
  this only after simulating `UNROLL_MAX_TRIP + 1 = 9` iterations that all
  continued. "Too big to unroll" is a stronger proof that the body runs than
  the `Ok` arm's.

The other two — a runtime bound, or a shape the analysis does not model — say
nothing about whether the body runs, and are refused.

Missing `TripOverCap` made the predicate answer `false` for every loop it was
written for, because the full unroller is default-ON and takes constant-trip
loops of 8 or fewer apart before LICM sees them. The population this permission
actually serves is **the constant-trip loop too big to fully unroll**, and that
population is reached only through the arm that looks like a refusal.

### The probe

`probes/CountedHoist.java`, written for this and for nothing else:
`static int walk(N o) { int a = 0; for (int i = 0; i < 1000; i++) a += o.v;
return a; }`. Static, so the base may be null and §3a's permission refuses it;
constant-bounded, so the body runs. Its `walkVar(null, 0)` half is the safety
check in the same file, so the two cannot drift.

| | ratio | floor | verdict |
|---|---:|---:|---|
| L5 — flag A/B on the optimizing tier | **0.793x** | 3.6% | **faster** |
| L6 — tier A/B with the flag on | **0.859x** | 1.2% | optimizing faster |

20.7% on the shape it was written for, which turns §6's "widening it is an
optimization" from a guess into a measurement. Checksum `60150000` on all 54
runs, and `walkVar(null, 0)` returns 0 in every arm — as does
`probes/ZeroTripHoist.java`, whose loop is bounded by a parameter and which this
permission therefore still refuses.

It stays **OFF by default** for the same reason §8's does: it is measured on the
probe written for it and nowhere else. CratonBench's checksums are byte-identical
with it on, so it is safe; safe is not the same as earning a default.

`CRATONVM_JIT_IR_LICM_HOIST_COUNTED`. It is a widening of a permission rather
than a fix to a wrong answer, which is the opposite direction from §3a's, so it
gets a flag where that one deliberately did not.

---

## 10. What is left, and what is deliberately not being done

§6's list, closed out. Two items were work; three were decisions, and saying so
is the point — an open-items list that only ever grows is not a list, it is a
backlog nobody reads.

| §6 item | now |
|---|---|
| `sumWide` is unmeasured | **done**, §7 |
| the four hoisted reads stay four | **done**, §8 |
| the speculation permission is narrow | **done**, §9 |
| the `getfield` cold arms are still inline | **not doing** — see below |
| nothing aligns a loop header | **not doing** — see below |
| the byte-span theory is dead | nothing to do; §2 reads as anatomy |

### The two that are decisions

**Outlining the `getfield` cold arms.** 61 of the loop's 412 bytes and one
taken branch per iteration. This is the same trade `CRATONVM_JIT_IR_POLL_OUTLINE`
made on a bigger block — 229 bytes and a taken branch — and it measured
**0.999x**: a correctly predicted branch over cold bytes costs about nothing,
because fetch follows the predicted target. Doing a smaller version of a
measured-nothing change, at a higher cost in the lowering, is not a judgement
call.

**Aligning a loop header.** There is no nop/pad emitter in this backend, so this
is new machinery before it is a measurement. §5's result is the reason to want
one less: what moved this loop was deleting work, not moving bytes.

### What is actually left

* ~~**The chain walk admits only reads.**~~ **Done — see §11.4**, and measured
  as changing nothing on every workload here. Not with LICM's private oracle
  either: `ir.rs` carries the canonical alias model and the walk asks that.
* **The merge is within one block.** `A` and `B` must share a control anchor,
  which costs every redundant read whose first copy is in a dominating block —
  the pattern `if (c) { … o.f … } … o.f …`. Lifting it needs a dominance query
  this pass does not have and `ir_schedule` does; wiring one in is a bigger
  change than this was.
* **Neither switch sets an `ir_evidence::Transform`.** A method whose only
  improvement is a removed read does not count as "this tier did something the
  baseline has no equivalent for", so the supersede gate will refuse to publish
  it. That is the conservative direction and it is deliberate for now: adding a
  `Transform` variant changes which bodies get published, and that is a
  measurement of its own.
* **The worst rows in `BENCHMARK.md` are not codegen.** String/Regex, HashMap
  and Binary Trees are allocation- and GC-bound. Nothing in this document or
  its predecessor touches them, and no amount of loop-body work will.

---

## 11. Three follow-ups, two censuses, and one number that changes what §8 means

§10 listed what was left. Three of its items were taken up; the interesting
part is that the first two answered each other, and the answer corrects §8.

### 11.1 The guard that repeats is not the one I said

The claim, made from an instruction count rather than a disassembly: four reads
of one receiver in one block carry four full guard sequences, and
`CRATONVM_JIT_IR_RECEIVER_GUARD_CSE` is default-ON and documented as "once per
receiver per block", so something is not reaching this shape.

Wrong. Reads 2, 3 and 4 have no `test rax, rax` at all — the receiver CSE works
exactly as documented. What repeats is two OTHER guards, and neither has a CSE:

```
1fc  cmp dword [rel ...],0      layout epoch guard   — per site
206  jne  ...
20c  mov rax,[rbp-70h]          receiver frame load  — per site
210  test rax,rax               null test  — ELIDED at sites 2, 3, 4
213  je   ...
219  test byte [rax+0Fh],4      per-object compact test — per site
220  je   ...
226  movsxd rax,[rax+10h]       the read
22d  jmp  ...                   over the legacy and helper arms
```

Site 1 executes 9 instructions and sites 2-4 execute 7 each: **30 per iteration
for four reads of one field**, where 12 would do.
`emit_cmp_layout_epoch_rip`'s own comment says the epoch guard is "emitted once
per INLINE FIELD ACCESS SITE and executed on every one of them", which is the
defect stated plainly in the place it happens.

**And eliding them is not the small change the count suggests.** Each guard
branches to THAT SITE's slow path — the checked helper for that field — and
then rejoins the fast path. So a later site cannot assume an earlier guard
passed: on the epoch-stale path, site 1 takes its helper, rejoins, and site 2
would then do an unguarded inline read at a stale offset. A real CSE needs a
block-level guard whose failure routes every site to a slow path, or a
duplicated tail. That is a different size of change, which is why the next
thing done was to size the population rather than start writing it.

### 11.2 The population, and why the answer is "not yet"

Two censuses were added for this, both process-global and monotone:
`ir_field_site_census` splits inline field-read sites by whether they were the
first in their block, and the existing `metrics::ir_getfield_declines` — which
nothing printed — now reports beside it.

| workload | inline field sites | later-in-block | null checks elided |
|---|---:|---:|---:|
| CratonBench | 4 | 2 | 2 |
| CratonBenchC2 | 18 | 7 | 7 |
| BinTreesClassic | 4 | 2 | 2 |

Two things fall out. **`elided` equals `later-in-block` in every row**, which
confirms 11.1 from a second direction: every repeat site does get its null check
elided. And the absolute numbers are 2 to 7 sites per workload, which does not
pay for block-level slow-path restructuring. **Not doing it**, on numbers rather
than on the guess that opened 11.1.

### 11.3 The number that changes what §8 means

§8 recorded load-CSE firing zero times on CratonBench and read that as "no
method in those kernels reads one cell twice out of one heap state in one
block". True, and misleading. The table above says the tier emits **4 to 18
inline field sites per workload** — there is barely any field-reading code
reaching the inline path at all, so the zero is a fact about how little of this
corpus the optimizing tier compiles with fields in it, not about redundancy.

The decline census is what rules out the other explanation: `ir getfield inline
declines` prints nothing on any workload, so **zero** sites were refused. The
inline path is not turning work away; the work is not there.

So §8's conclusion survives — the switch stays OFF — but its *reason* is
narrower than written: **this corpus cannot answer whether load-CSE matters for
real Java.** A field-dense workload (H2, Hibernate) would, and neither corpus is
in this tree; `tools/h2-ab/h2-ab.sh` exists and its own header records that the
H2 corpus it was written for is gone too. That is the measurement to take before
anything else is built on this pass.

### 11.4 The alias widening, implemented and measured as nothing

§10 proposed letting the chain walk step over a store that provably cannot
alias the cell being read. `CRATONVM_JIT_IR_LOAD_CSE_ALIAS`, default OFF.

It does not need LICM's private oracle, which is what §10 pointed at: `ir.rs`
carries the canonical model — `AliasClass`, `AccessOffset::provably_distinct`,
`Graph::may_alias`, `effect_of_node` — and the walk now asks it directly. Two
different field indices are two different cells whatever the bases are: one
object cannot hold one cell at two indices, and two objects have disjoint
storage. Three things still stop the walk regardless: a safepoint (a relocating
collector may run there, and this pass DELETES a read and hands an older value
forward, which is a different claim from `MemEffect`'s "a load may cross a
safepoint freely"), an allocation, and any ordering stronger than
`MemOrder::Plain`.

One implementation note worth keeping, because the failure mode is silent. The
first version used the bare `effect_of_node`, which leaves an offset operand as
`AccessOffset::Dynamic`; `provably_distinct` only ever answers `true` for a pair
of unequal CONSTANTS, so nothing was ever disjoint and the widening did exactly
nothing. Its negative test passed and its positive test failed, which is the
right way round to find out. `Graph::memory_effect` is the graph-aware form that
folds constant offsets, and it is the one to call.

**Measured delta: zero.** Same reads removed in both arms on every workload —
0, 0, 0 and 9. It is a correct widening with no demonstrated target in this
corpus, kept OFF and recorded, for the same reason
`CRATONVM_JIT_IR_POLL_OUTLINE` was.
