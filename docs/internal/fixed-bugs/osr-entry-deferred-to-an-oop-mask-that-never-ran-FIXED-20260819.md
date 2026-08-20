# OSR entry was refused for a slot the oop mask had no opinion about — `BOBYQAOptimizerTest` HANG → PASS, 24x on the kernel

**Status: FIXED 2026-08-19** on `fix/commons-math-residuals-20260819`, default
ON, `CRATONVM_JIT_NO_OSR_REFINED_REF=1` is the kill switch.

`osr_refused_entry` **5 706 → 0**, `BobyqaOne 12 1` **22.8-26.3 s → 1.0-1.4 s**
(four interleaved pairs, all four favouring the fix), and
`optim.nonlinear.scalar.noderiv.BOBYQAOptimizerTest` **≥400 s HANG → 14.2 s
PASS, 17/17**. HotSpot runs the same class in 11.8 s on the same host, so this
closes a gap that was quoted as 80x down to **1.2x**.

The printed result is bit-identical across all three configurations —
`value=6.634794878318594E-15` on HotSpot, on the fix, and on the kill switch —
so nothing was skipped to get the number.

## What this replaces

`known-issues/perf/bobyqa-numeric-kernel-is-80x-slower-than-hotspot-20260817.md`
spent two days and four hypotheses on this workload and reached, in its own
words:

> The process is not deadlocked and is not stuck in the interpreter — it is
> running compiled code, correctly, about eighty times too slowly.

Both halves are false on current `dev`. A fresh `perf record` of
`probes/BobyqaOne.java`:

| | share |
|---|---:|
| DSO: **VM binary** | **94.2%** |
| DSO: JIT-compiled code | **0.67%** |
| `interpreter::execute_frame_from_index` | 17.3% |
| `interpreter::opcodes::op_getfield` | 11.2% |
| `interpreter::dispatch_virtual::execute_invokevirtual_cached` | 7.5% |

Those are interpreter symbols. The page's own profile — "66.1% JIT-compiled
code / 31.2% VM binary" — was taken on `probes/AccessorDispatchProbe.java`, the
shape-only microbench it built precisely because *"a 45-second `optimize()` call
cannot be iterated against"*, and then read as if it described the workload.
**The instrument was a different program.** That is the whole error, and it is
why the page's eventual verdict (the ZGC JIT load barrier, 3-5 weeks, gated on
another workstream) pointed at something that would have moved this by ~8%.

## The defect

`BOBYQAOptimizer.trsbox` and `bobyqb` are called once per test, so OSR is their
only compile door. Both compile. Neither can be ENTERED:

```
osr_entered=275   osr_refused_entry=15152     hot_but_stuck_in_interpreter=0
```

`hot_but_stuck_in_interpreter=0` is why no admission census ever flagged this:
the methods are not stuck waiting to compile, they are compiled and unreachable.

`CRATONVM_DBG_JITC=1` names the refusals, and they are all one slot:

```
4306  trsbox  osr-entry-unresumable-exit
              (deopt point at bci 2827 (OsrExit): local 87 (Unsupported))
 898  trsbox  osr-entry-undescribable-slot (local 87)
 310  bobyqb  osr-entry-unresumable-exit   (bci 607 ReceiverTypeChanged: local 57)
```

`osr_exit_policy` is an **artifact-wide veto**: one `FrameValue::Unsupported`
slot at one deopt point refuses OSR entry at *every* back edge of the method.

`javap` says what local 87 is. It is stored as three different kinds:

| kind | accesses |
|---|---|
| `double` | 19 `dload`, 10 `dstore` |
| `int` | 7 `iload`, 1 `istore`, 1 `iinc` |
| reference | 1 `aload`, 1 `astore` |

`bobyqb`'s local 57 is the same shape (12 `dload`/6 `dstore`, 43 `iload`,
3 `aload`/2 `astore`). This is ordinary javac slot reuse; `BOBYQAOptimizer` is a
machine translation of Powell's Fortran and its methods carry 90+ locals.

So `classify_local_kinds` correctly calls the slot `Ambiguous` for the method,
and the per-bci refinement is exactly the mechanism meant to resolve it.

## Why the first fix moved nothing, and the diagnostic that said so

The obvious reading is "the refinement did not settle, so publish `Undefined` —
JVMS 4.10.1.6 merges two concrete kinds to `top` and 4.10.1.9 makes a `top`
local an illegal operand of every load, so nothing reachable can read it". That
was implemented (`CRATONVM_JIT_NO_OSR_AMBIGUOUS_DEAD`), and `osr_refused_entry`
came back **5706 in both arms** — identical to four significant figures.

The reason needed an instrument, not another argument.
`AmbiguousLocalKinds::kind_at` collapses two different facts into `None`:
`Ambiguous` (the dataflow reached this pc and two kinds arrive) and `Unknown`
(it never reached the pc). `raw_at` reports the unfiltered state, and
`CRATONVM_DBG_OSR_SLOTS=1` prints it:

```
[osr-slot] UNSUPPORTED local=87 bci=2827 whole_method_kind=Some(Ambiguous)
           refined=None raw=Some(Ref) cfg_exact=true
           reg=None xmm=None spill_off=704 live_covered=true
```

**`raw=Some(Ref)`.** The dataflow settles the slot as a *reference* at every
blocking bci. `kind_at` was filtering that answer out, on stated grounds:

> Never answers `Ref`: the flow-sensitive oop mask is the sole authority for
> ref-typed slots and has already had its say.

## The actual bug: deferring to silence

At these bci the mask has not had its say. `CRATONVM_DBG_EXCFRAME=1` over the
same run:

```
trsbox bci=0     oop_reached=false oop_mask=0x0
trsbox bci=374   oop_reached=false oop_mask=0x0
trsbox bci=1212  oop_reached=false oop_mask=0x0
...  (every snapshot in the method)
```

The precise oop-mask dataflow **declined outright** for this method. And even
had it run, the mask is a single `u64` — it cannot address slot 87 at all. The
comment sitting directly above the mask read in `deopt_stubs.rs` already says
both things, and already draws the conclusion:

> `classify_local_kinds` has no 64-slot cap … its `LocalKind::Ref` arm below
> publishes `RegisterRef`/`StackSlotRef` at ANY slot index. **It — not this
> mask — is the reference authority above slot 63.**

`kind_at` was deferring to an authority that could not answer, and
`oop_reached=false oop_mask=0x0` is silence, not a negative verdict.

## The fix

When the per-bci dataflow settles a slot as `Ref` **and** the oop mask has no
opinion at that bci (`!oop_reached || slot >= 64`), publish
`RegisterRef`/`StackSlotRef` — exactly what `typed_local_frame_value`'s
`LocalKind::Ref` arm already publishes, for the reason it already gives:

> the JVM verifier's definite-assignment rule means that same not-provably-live
> slot can NEVER be legally read by any bytecode reachable from here … so for
> THIS resume-snapshot purpose specifically — unlike the GC root scan —
> trusting the single unambiguous scan classification is sound.

The premise here is *stronger* than the one that arm already accepts: that arm
trusts a whole-method scan ("`Ref` everywhere it is ever accessed"); this is a
flow-sensitive answer at this pc.

### The one condition that is load-bearing

`AmbiguousLocalKinds` now carries `has_jsr`, split out of `cfg_is_exact`,
because only one half of CFG-exactness matters for trusting a SETTLED kind:

* **`jsr`/`ret` — checked.** `oop_dataflow_successors` gives `ret` no
  successors, which makes the graph NARROWER than the verifier's and could
  settle a kind the verifier would merge further.
* **Exception handlers — deliberately not checked.** Handler entries are seeded
  `Ambiguous` (TOP) because a handler is reachable from any pc in its protected
  range. That is COARSER than the verifier's merge, so it can only turn a
  settled kind into `Ambiguous`, never manufacture one. Folding it in would
  refuse every method that merely has a `try` block, for no soundness gain.

`cfg_exactness_tracks_handlers_and_jsr` pins both directions and was verified by
breaking it: deleting the `exception_ranges.is_empty()` conjunct makes the
handler case read `true` and the test fails.

## Measurement

One binary, kill-switch A/B, so no cross-binary comparison is involved.

| | OFF (`CRATONVM_JIT_NO_OSR_REFINED_REF=1`) | ON (default) |
|---|---:|---:|
| `osr_refused_entry`, `BobyqaOne 8 1` | 5 706 | **0** |
| `osr_entered`, same run | 223 | 1 |
| `BobyqaOne 12 1`, rep 1 | 24.6 s | **1.0 s** |
| rep 2 | 22.8 s | **1.0 s** |
| rep 3 | 26.3 s | **1.3 s** |
| rep 4 | 25.5 s | **1.4 s** |
| `BOBYQAOptimizerTest` | **≥400 s (rc=124)** | **14.2 s, ok=17/17** |

HotSpot 25 on the same (shared, load ~8) host: `BobyqaOne 12 1` 0.74-0.81 s, the
class 11.8 s.

`osr_entered` falling 223 → 1 is not a regression and is the same fact as the
refusal count: once the entry is admitted the method stays in compiled code
instead of bouncing back to the interpreter and re-requesting entry at the next
back edge, thousands of times.

## What it did NOT fix, said plainly

The sibling `Ambiguous → Undefined` relaxation shipped in the same branch
(`CRATONVM_JIT_NO_OSR_AMBIGUOUS_DEAD`) is correct and separately argued, and it
is worth **zero** on this workload — every blocking slot here settles `Ref`, not
`Ambiguous`. It is kept because it closes a real gap that will fire elsewhere,
not because it was measured to help here.

## Reproduction

```bash
CP=$(cat /data/cm-legacy-classpath.txt)   # Azure Linux box
javac -nowarn -cp "$CP" -d /tmp/probes probes/BobyqaOne.java

# the counter, which host load cannot move
CRATONVM_DBG=jit-method-stats cratonvm --java-home <jdk-25> --Xmx 1g \
  -c "/tmp/probes:$CP" BobyqaOne 8 1 2>&1 | grep 'OSR lifecycle'

# the refusal reasons, one line per refusal
CRATONVM_DBG_JITC=1 cratonvm ... BobyqaOne 8 1 2>&1 | grep OSR-refuse

# the slot, and which of the three answers applies
CRATONVM_DBG_OSR_SLOTS=1 cratonvm ... BobyqaOne 6 1 2>&1 | grep osr-slot

# whether the oop mask ran at all
CRATONVM_DBG_EXCFRAME=1 cratonvm ... BobyqaOne 6 1 2>&1 | grep 'FRAME.*trsbox'

# the A/B, one binary
CRATONVM_JIT_NO_OSR_REFINED_REF=1 cratonvm ... BobyqaOne 12 1   # slow arm
cratonvm ... BobyqaOne 12 1                                     # fast arm
```

## Transferable

1. **A microbenchmark built because the real workload is too slow to iterate
   against is not a profile of the real workload.** The 80x page built
   `AccessorDispatchProbe` for exactly that reason, said so, and then quoted its
   DSO split as the workload's. One `perf record` of the workload itself — 40
   seconds — would have refuted the page's central sentence on day one.
2. **`hot_but_stuck_in_interpreter=0` does not mean "not in the interpreter".**
   It means "no method is waiting to be compiled". A method that compiles and
   cannot be entered is invisible to it, and `osr_refused_entry` is the counter
   that sees it.
3. **When a census collapses two states into one answer, it will eventually
   cost a fix.** `kind_at` returning `None` for both `Ambiguous` and `Ref` cost
   one full build cycle spent implementing the wrong fix — which then reported
   an identical number in both arms, the signature of a change that never fired.
4. **A gate that defers to another component must check that the component
   answered.** `oop_reached=false` and "the mask says no" are different facts,
   and the code had no way to tell them apart.
