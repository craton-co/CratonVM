# The optimizing tier's loop body is mostly code it never runs

**Status:** one default flipped, one wrong answer fixed, two switches left OFF
because they measured as nothing.
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
* **`sumWide` is unmeasured.** `probes/FieldLoop.java`'s four-accumulator arm
  exists to separate a latency bottleneck from extra work, and every number here
  is from `sum`. The memory-edge hoist should help it more (four reads become
  one), which is exactly why it should be checked rather than assumed.
* **The speculation permission is narrow on purpose** (§3a): receiver, or
  `definitely_non_null`, or already anchored at the header. A loop whose bound
  is provably positive, or a base proven non-null by a dominating check, is
  hoistable and is refused today. Widening it is an optimization; each widening
  is a new claim about when the body must run.
* **Nothing aligns a loop header**, and there is no nop/pad emitter in this
  backend at all. Worth less than it looked like before §5, for the same reason
  as the first bullet.
