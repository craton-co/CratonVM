# `String.charAt`: lifting the pin buys 3x, HotSpot is still 80-100x away, and the pin is installed at a door this population never takes

**Status:** OPEN. **Measured.** The fix is real, it is ~3x, and it does not
close the gap — and the reason it does not is now known.

* **Measured** 2026-09-01 on `claude/audit-impl-20260901` @ `32f7d47c3`,
  Windows, 32 cores, JDK 25.0.3 as the oracle, `probes/CharAtCostCurve.java`.
  Lifting the pin takes `charAt` from ~336 ns/char to 59-108. HotSpot on the
  same host and probe is 1.1-1.4. **The residual gap is ~80-100x.** Do not read
  this page as saying the `charAt` cliff is fixed.
* **The page's open question is answered, and by none of its five hypotheses.**
  All five asked what was different about the *method*. The discriminator is
  the **door** — see [The answer is the door](#the-answer-is-the-door). The pin,
  the eligibility conjunction, the four counters and the layout publish all live
  in `try_compile_inner`, and the methods that motivated them are compiled
  through `CompileDoor::Osr`, which reaches the backend directly.
* **Superseded in part.** The 186-196 ns/char table further down was taken on
  `dev` @ `56d6c3722`, x86-64 Linux, 8 cores. Different host, OS, core count and
  binary from the new table: read it for its *shape* — flat, `charAt`-not-loop,
  36x the `char[]` loop — and never arm-for-arm against the numbers above it.
* **A fix for the door defect is in flight** as of this writing (agent C1,
  `jit/src/lib.rs` and `jit/src/compile_gate.rs`). Nothing on this page
  describes that fix, because nothing on this page has verified it. Every code
  reading below is "as of `32f7d47c3`, working tree clean".

## The measurement

`probes/CharAtCostCurve.java`, the `charAt` rows only, ns/char. One binary,
four arms, same host, same run of the probe.

| reps | HotSpot 25.0.3 | A: default | B: `NO_STRING_INTRINSIC_PIN=1` | C: pin off, emitter off |
|---:|---:|---:|---:|---:|
| 2 | 56.84 | 339.96 | **59.45** | 248.57 |
| 10 | 1.11 | 337.78 | **62.67** | 263.33 |
| 50 | 1.40 | 331.85 | **66.60** | 268.80 |
| 200 | 1.08 | 335.99 | **66.30** | 421.03 |
| 200 | 1.22 | 322.44 | **83.71** | — |
| 200 | 1.08 | 313.96 | **106.81** | — |
| 1000 | 1.37 | 312.05 | **107.73** | — |

* Arm A — default. Pin ON, so the IR String-access expander is unreachable.
* Arm B — `CRATONVM_JIT_NO_STRING_INTRINSIC_PIN=1`. This is now the whole arm;
  the second flag this page used to demand is no longer needed, see
  [Three gates, and gate 2 is fixed](#three-gates-and-gate-2-is-fixed).
* Arm C — `CRATONVM_JIT_NO_STRING_INTRINSIC_PIN=1
  CRATONVM_JIT_IR_STRING_INTRINSICS=0`. Same routing as B, emitter off. It is
  the control for B: **B-minus-C is the emitter and nothing else.**

Three things to read off it, each of them plainly:

**1. The IR String-access expander works. Arm B is 3-5x arm A.** 336 → 66 at
matched rep counts, and the effect is present at every rep count measured. That
is the recommendation-3 emitter running, and it is the largest single
improvement anything on this page has produced.

**2. Lifting the pin *without* the emitter is not a win, and at the largest rep
count it is a loss.** Arm C never leaves arm A's band, and at reps=200 it reads
**421 against arm A's 336**. So the 3x is the emitter, not the routing —
routing a String-accessor method to the IR tier and dropping its intrinsic is
what `string_intrinsic_pin_enabled`'s own in-tree measurement already said it
was:

> ```
> C2/IR body, intrinsic dropped   504 ns/call
> C1 body, intrinsic emitted      135 ns/call   (3.7x)
> ```

That measurement was taken on `java/nio/StringCharBuffer.get()C` and has now
been reproduced, independently and on a different probe, by arm C.
**The pin's decision is correct.** What was wrong with it was never the
decision; it is where the decision is installed.

(Honesty about arm C: it is four rows, it is noisier than the others, and at
reps 2/10/50 it reads *below* arm A. The claim it supports is "arm C is arm A's
family, not arm B's" plus the reps=200 inversion — not a clean monotone loss.)

**3. It is an improvement, not a fix.** Arm B's 59-108 against HotSpot's
1.08-1.40 is still **~50-100x**. A `charAt` loop on this VM remains two orders
of magnitude off the oracle after the best arm this branch can produce.

## The answer is the door

The page recorded five hypotheses, each refuted by its own measurement, none
converging. The sixth thing — the one none of the five tested — has now been
measured.

**The pin's own census, on arm A, on the workload the pin exists to govern:**

```
[cratonvm] JIT String-intrinsic pin: fired=0 blind-no-layout=0 blind-no-resolver=0 fail-closed=0
```

All four counters zero. `fired=0` was the reading this instrument was built to
make legible, and `blind-no-layout=0` rules out the leading hypothesis the
counter was added for — the layout not resolving. The pin did not decline, and
it was not blind. It was **not asked**.

`CRATONVM_DBG=jitc` on the same run says why:

```
[cratonvm-jitc] bg-compile CharAtWarmShape.scanBig(Ljava/lang/String;I)I tier=C2 optimized=true osr_bci=12
[cratonvm-jitc] OSR-compile CharAtWarmShape.scanBig(Ljava/lang/String;I)I entry_pc=12 entry=0x... len=3611
```

These methods are compiled through the **OSR door**. And `jit/src/compile_gate.rs`'s
own module header — the file that exists because this exact failure has happened
three times already — states the consequence in a table:

| Door | Where | Reaches the backend via |
|---|---|---|
| `CompileDoor::MethodEntry` | `try_compile_with_invokespecial_resolver` | `try_compile_inner` → `x64::compile_with_param_slots` |
| `CompileDoor::EagerFirstCall` | `execute`'s first-call compile (`vm/src/runtime/interpreter.rs`) | `x64::compile_with_param_slots` **directly** |
| `CompileDoor::Osr` | `compile_osr_artifact` (`vm/src/runtime/interpreter/jit_bridge.rs`) | `x64::compile_with_param_slots` **directly** |

Four code facts, read at `32f7d47c3` with the working tree clean, that together
say the population and the machinery are at different doors:

1. **The pin is at one door.** `string_intrinsic_pin_declines` — which is also
   where all four counters are incremented — has exactly one production call
   site: the eligibility conjunction in `try_compile_inner` (`jit/src/lib.rs`
   ~21175). Nothing on the `Osr` or `EagerFirstCall` paths calls it, and
   `compile_gate::admit` does not ask it.
2. **The IR tier is at the same one door.** `IrBuilder::build` / `ir_lower` are
   driven from `try_compile_inner`. The two direct doors call
   `x64::compile_with_param_slots`, which is the single-pass backend. An
   OSR-door body is single-pass **by construction** — so at that door there is
   nothing for the pin to pin, and no IR expander to reach.
3. **The emitter's arming is at the same one door.** `ir::publish_string_layout`
   has exactly one caller in the tree: `jit/src/lib.rs:20954`, inside
   `try_compile_inner`. The OSR door resolves its *own* `osr_string_layout` and
   hands it to `compile_with_param_slots` (`jit_bridge.rs` ~1241) but never
   publishes it. So the IR expander's arming is a side effect of some *other*
   method's method-entry compile: a process whose String-accessor methods all
   arrive through OSR arms it with nothing, and `[ir-string]` reports
   `no_layout=N, emitted_charAt=0` regardless of the flags.
4. **The OSR door does register the single-pass String intrinsic itself.** It
   calls `try_resolve_string_intrinsic` with `osr_string_layout` (~1241). So the
   OSR door is not intrinsic-blind — it is *pin*-blind and *expander*-blind.

**Every one of the five hypotheses was about the method. The discriminator was
the door.** That is the transferable lesson, and it is the reason
`CRATONVM_JIT_NO_STRING_INTRINSIC_PIN=1` read as inert in the original audit
(326.3/328.6 against 329.5/333.7): at the OSR door the flag governs a decision
nobody was making. A null A/B result and a gate that is never consulted are the
same observation from outside — which is what the census was built to separate,
and it did.

This codebase has been bitten by the same shape before, every time expensively:
the `iinc_w` scan refusal (a bail at one door left `FastLz.compress` interpreted
at 138x), the `checkcast` OSR door (v1 patched single-pass only), the thin
native bind (four doors, one patched). `compile_gate.rs` was written to stop it,
and its own header says the gate covers *admission* — it does not cover the
per-door metadata each door builds for itself, which is the class this defect
belongs to (the header already names one member of that class, `H20-1`, the
direct-call plan).

### The two-probe split — RECONCILED, settled by a run

Stated because it is load-bearing for whoever picks this up, and because the
page's own rule is to be explicit about what is measured and what is reasoned:

On the current source, `CRATONVM_JIT_NO_STRING_INTRINSIC_PIN=1` can only change
*behaviour* through `string_intrinsic_pin_declines` at `jit/src/lib.rs` ~21175
— the same function that increments `STRING_PIN_FIRED`. (The other two readers
of `string_intrinsic_pin_enabled` are the pure classifier
`string_intrinsic_pin_verdict`, used by the printed verdict chain, and the
subordinate fail-closed rule inside the counted function itself.) So a compile
where the flag mattered is a compile where `fired` was incremented on the arm-A
side. **`fired=0` and a 3x flag effect cannot describe the same compile.**

The most likely reading is that they describe different ones: the census and
`jitc` excerpts above name `CharAtWarmShape.scanBig`, while the timing table is
`CharAtCostCurve` — two probes, and the whole point of this section is that two
probes can take two doors. **That is a reading, not a measurement.** The
settling run is one line:

```sh
CRATONVM_DBG_JIT_METHOD_STATS=1 cratonvm -cp probes CharAtCostCurve 2>&1 | grep 'String-intrinsic pin'
CRATONVM_DBG_IR_STRING=1 CRATONVM_JIT_NO_STRING_INTRINSIC_PIN=1 \
  cratonvm -cp probes CharAtCostCurve 2>&1 | grep '\[ir-string\]' | tail -1
```

**That run has now been made, and it settles it.** On the built branch,
`claude/audit-impl-20260901` @ `32f7d47c3`, Windows, same binary as the timing
table:

```
$ CRATONVM_DBG=jit-method-stats cratonvm -cp probes CharAtCostCurve
[cratonvm] JIT String-intrinsic pin: fired=2 blind-no-layout=0 blind-no-resolver=0 fail-closed=0

$ CRATONVM_DBG=jitc cratonvm -cp probes CharAtCostCurve
[ir] admission CharAtCostCurve.scan(Ljava/lang/String;I)I: pinned to the single-pass
    backend: it has a String access intrinsic here and the IR tier has none
[cratonvm-jitc] full-compile CharAtCostCurve.scan(Ljava/lang/String;I)I entry=0x... len=6104
```

`fired=2`, and the door is `full-compile` — the **method-entry** door, not OSR.
So the reading was right and the derivation above is sound:

* **`CharAtCostCurve` takes the method-entry door.** The pin is asked, it fires,
  and lifting it is what buys arm B's 3x. The timing table measures a real pin
  effect.
* **`CharAtWarmShape` takes the OSR door.** The pin is never asked, which is why
  its census reads `fired=0`.

The two are different probes taking different doors, and **both findings stand
independently**. What this rules out is the alarming version — that the flag was
moving something nobody had located. There is no fourth reading site.

What it does **not** rule out is the door gap itself: a loop-hot String method
compiled through OSR still never consults the pin, so whether it gets the
intrinsic is decided by whatever that door happens to do rather than by the
policy this VM wrote down. That is the open half, and the per-door census added
in the same commit is what makes it a one-run question from here on.

## Five hypotheses, five refutations — and why all five failed

Every plausible discriminator was tested and each is ruled out by its own
measurement. This section was already the value of the page; it is worth more
now, because there is finally an explanation for the pattern. **All five are
about the method. None of them varies the door**, and `OsrVsEntry` — the one
that sounds like it does — varies *how the method is entered*, not which
compile door produced the installed artifact.

| hypothesis | test | result |
|---|---|---|
| OSR entry is slower than method entry | one long call vs 2,000 short ones (`OsrVsEntry`) | 239 vs 205 ns/char — **no** |
| the callee was cold when the caller compiled | make `charAt` hot in a third method first, then run | 327 vs 291 ns/char — **no** |
| the caller's shape decides it | six callers: direct, static helper, `LongSupplier`, user-declared interface | 312-343 ns/char, all six — **no** |
| the first compile is final, and its context decides | two identical methods, one first entered from `main`, one from a helper, then interleaved reruns (`probes/CharAtFirstCompile.java`) | 278-324 ns/char for both, all eight rows — **no** |
| it is a scale threshold | 200k → 100M characters in one call | flat 186-196 — **no** |

## Arm B rises with scale, and HotSpot does not

Named as an open question, not answered.

Arm B is **59.45 at reps=2 and 107.73 at reps=1000**, monotonically increasing
across the seven rows, while HotSpot is flat at 1.08-1.40 across the same rows
and arm A is flat-to-slightly-falling (340 → 312). A flat line was the original
finding's strongest single piece of evidence — it is what said "whatever body
runs at 200,000 characters is still running at 100 million". A *rising* line is
a different shape and it is not explained by anything on this page.

Candidates, none tested:

* **deopt churn.** The expansion guards null receiver, null `value` and
  out-of-range index with `Op::Guard { bci }`, and every guard that trips
  re-executes the `invokevirtual` in the interpreter. A guard that fires at some
  low rate would cost more as the loop runs longer only if the rate rises —
  worth checking before assuming it does not.
* **the receiver guard**, on any site the expander did not refuse.
* **LICM not hoisting `value` and `coder`.** The emitter's own design note says
  the win depends on `ir_optimize` hoisting those two `Op::Load`s out of the
  counted loop, and records that **whether it does is a measurement nobody has
  taken**. If they are hoisted the body is an element read; if they are not, it
  is two field loads plus the decode, every character.

The next measurement is `CRATONVM_DBG_IR_GRAPH=1` on the arm-B compile of
`CharAtCostCurve.scan`, reading whether the `value`/`coder` loads are inside or
outside the inner loop, plus the guard/deopt count across the rep sweep. Answer
that before touching the emitter.

## The prior Linux measurement, kept for its shape

`dev` @ `56d6c3722`, x86-64 Linux, 8 cores, `/proc/loadavg` 5-11 per run,
real-JDK mode, default flags. Three loop bodies at growing inner rep counts over
a 100,000-char `String` and the `char[]` from the same text.

| body | reps | CratonVM | HotSpot 25 |
|---|---:|---:|---:|
| `s.charAt(i)` | 2 | 190.0 ns/char | 41.6 |
| | 10 | 189.7 | 0.75 |
| | 50 | 227.5 | 0.50 |
| | 200 | 186.1 | 0.52 |
| | 1000 | 195.6 | 0.49 |
| `charAt`, `length()` hoisted | 200 | 155.8 | 0.47 |
| | 1000 | 177.1 | 0.46 |
| `a[i]` on a `char[]` | 200 | **5.38** | 0.12 |
| | 1000 | **5.22** | 0.12 |

* **It is flat**, from 200,000 characters to 100,000,000.
* **It is `charAt`, not the loop.** The identical loop over a `char[]` is
  **36x cheaper in the same VM**. Not bounds checks, not the induction variable,
  not the compare. The call.

### A 3 ns path exists in the same binary

`probes/CharAtWarmShape.java` runs a byte-identical body and prints:

```
CratonVM   warm3x50    3.44, 2.96 ns/char
           warm3x200   3.31, 3.46 ns/char
HotSpot    warm3x50    0.25       warm3x200  1.24
```

Same binary, same class file, same method body, same host — and **still ~20x
faster than arm B's best**. So the single-pass inline `charAt` lowering
(`jit/src/x64/bytecode_walk.rs`, a coder-branch decode with no `CALL`) remains
the fastest thing this VM has for `charAt`, and the IR expander has not matched
it. That is a target, and it is a reason to be careful with any retirement that
would take the single-pass body away.

(The probe's own class comment says dropping the warm reps to 50 "loses the fast
body entirely (measured 340 ns/char)", which contradicts the `warm3x50` row.
Its two arms are *separate methods* — `scanSmall` and `scanBig` — so the comment
describes a run that is not the one recorded here. Still unreconciled; believe
the table.)

## What changed on 2026-09-01

Four items on `claude/audit-impl-20260901`, all now **built and run** — the
numbers above are theirs.

### 1. The printed verdict now contains the pin — DONE

`string_intrinsic_pin_verdict` (`jit/src/lib.rs`) is a pure classifier called by
**both** the printed verdict chain and the real eligibility conjunction, so the
reason a run reports and the decision a run takes cannot drift apart. Four
states: `Pinned` (its own refusal arm), `DisabledByFlag`, `BlindNoLayout`,
`BlindNoResolver` — the last three admitted **with a note appended** to the
admission line rather than replacing it, so `metrics::CompilationReport::admission`
keeps meaning "admitted".

`BlindNoLayout` is narrowed to methods calling something declared on
`java/lang/String` or `java/lang/CharSequence`, so the diagnostic is evidence
rather than noise.

The instrument did its job: it is what turned "the pin does not help" into "the
pin was never asked". It could not, and cannot, say *which door* did the compile
— that is the gap the next revision of this machinery should close.

### 2. The pin fails closed in ONE of the two blind cases, deliberately

**Layout `None` → still fails OPEN, counted.** `try_resolve_string_intrinsic`
tests the class name and only then reaches `let layout = string_layout?;`, so
with `None` it returns `None` for *every* site and the single-pass backend emits
no inline decode either. Pinning would buy a C1 body with no intrinsic in place
of a C2 body — a pure downgrade.

**Resolver `None` → fails CLOSED**, and only when a layout *did* resolve. That
is a missing input at one door, not evidence about the method. The cost was
checked: the single-pass path takes `jitc_bail!("cp_invoke_resolver")`, which
does not set `backend_attempted`, so the method is not permanently bail-listed —
worst case one abandoned compile, retried later. The asymmetry matters because
**a compiled body is installed and kept**: an intrinsic-less body made at a
blind door is the body for the life of the process.

Kill switch: `CRATONVM_JIT_NO_STRING_PIN_FAIL_CLOSED=1`
(`CRATONVM_JIT=string-pin-fail-closed`, default-ON opt-out), subordinate to the
pin so the two A/B arms differ by one thing.

### 3. Four counters, and a zero that is readable — DONE, and it paid

`string_intrinsic_pin_census() -> (fired, blind-no-layout, blind-no-resolver, fail-closed)`,
printed by `dump_method_stats_to_stderr` (`jit/src/tiered.rs`) as

```
[cratonvm] JIT String-intrinsic pin: fired=N blind-no-layout=N blind-no-resolver=N fail-closed=N
```

under `CRATONVM_DBG_JIT_METHOD_STATS=1` (or `CRATONVM_DBG=jit-method-stats`).
The all-zero reading above **is** the finding of this revision.

Two caveats, both from the code and both still true:

* The line sits **after** the `DIAG_CORE` early return. A run that never built a
  tiered manager prints no line at all — absence of the line is not `fired=0`.
* `blind-no-resolver` was **predicted to read zero** and does. All three
  production doors (`vm/src/runtime/interpreter/jit_bridge.rs`, ~5871, ~6090,
  ~7659) pass `Some(&invoke_resolver)`; a non-zero reading would name a door
  nobody knew existed. They also pass `Some(&string_layout_resolver)`, so a
  non-zero `blind-no-layout` is a fact about what that **resolver returns**,
  never about a door that forgot it. Note what the zero does *not* cover: a door
  that never calls the pin at all is invisible to all four counters.

### 4. The IR tier has a String-access emitter — DONE, and it is the 3x

`jit/src/ir.rs` gained `length()`, `isEmpty()` and `charAt(int)`, expanded at
IR-build time into **existing primitive nodes** — no new `Op`. Shape (a), new
`Op::StringCharAt` variants lowered in `ir_lower`, was blocked by evidence:
`jit/src/ir_verify.rs`'s `expected_arity` is an exhaustive `match` over `Op`
with no wildcard arm, so a new variant is a compile error in a file the change
could not touch. Shape (b) is better on merit anyway — `value` and `coder`
become ordinary `Op::Load`s the optimizer can see.

What it emits (compact-String representation, as the single-pass region assumes):

```
coder  = Op::Load(Int)   [ctrl, mem, recv, Const(coder_field_index)]
value  = Op::Load(Ref)   [ctrl, mem, recv, Const(value_field_index)]
len    = Op::ArrayLength [ctrl, mem, value]
count  = Op::UShr(len, coder)
charAt: off = Op::Shl(index, coder)
        lo  = Op::ArrayLoad(Byte)(value, off)         & 0xFF
        hi  = Op::ArrayLoad(Byte)(value, off + coder) & 0xFF
        ch  = lo | ((hi << 8) & (0 - coder))
```

Details worth keeping:

* the decode is **branchless** where the single-pass region uses a
  `TEST coder; JNZ utf16` diamond, because a diamond needs new `Op::If` /
  `Op::Merge` at a pc that is not a branch target;
* `try_resolve_string_intrinsic` is **called, not re-derived**, so the
  receiver-guard policy stays the single-pass backend's own;
* the IR tier **refuses the guarded (`CharSequence`) case** — no IR node reads
  an `ObjectHeader` class id, and `Op::InstanceOf` is an opaque helper `CALL`
  that would sit in the loop body. Emitting unguarded would admit a
  `StringBuilder` receiver to a String-layout decode: a wrong **value**, not a
  slow one. Counted as `guarded_site_refused`;
* narrow-oop safety comes from **never emitting a displacement**: the expansion
  reads `value_field_index`, `coder_field_index`, `has_coder` and
  `string_class_id` — slot indices and identities, never a byte offset. (The
  byte offsets in `StringFieldLayout` are *not* process-stable.)
* `coder` is loaded **before** `value`, because the `Op::Load` helper arm is a
  `CALL` and loading the `Ref` last is the shortest window with a live oop
  across one;
* every uncertain case **deopts, none throws**: null receiver, null `value` and
  out-of-range index reach `Op::Guard { bci }` at the invoke's own bci, where
  `IrBuilder::build`'s snapshot still holds receiver (+index) on the operand
  stack. The interpreter re-executes the `invokevirtual` and the native raises
  the exact `NullPointerException` / `StringIndexOutOfBoundsException`;
* the expansion **refuses inside an open splice** (`in_splice_refused`) — a
  spliced region pushes no snapshot, the shape behind
  `ir-inline-turns-an-index-out-of-bounds-into-an-internalerror-FIXED-20260828`.

Census: `ir_string_intrinsic_census()` over 10 outcomes — `sites_seen`,
`flag_off`, `no_layout`, `not_an_accessor`, `guarded_site_refused`,
`in_splice_refused`, `shape_refused`, `emitted_length`, `emitted_isEmpty`,
`emitted_charAt`. Nothing in `vm-cli` calls it; the only way to read it is the
per-decision line under `CRATONVM_DBG_IR_STRING=1`, which is cumulative, so
`tail -1` of the `[ir-string]` lines IS the exit census.

Kill switch: `CRATONVM_JIT_IR_STRING_INTRINSICS=0`
(`CRATONVM_JIT=ir-string-intrinsics`), **default ON** — the pin keeps these
methods off the IR tier at the door where it is asked, so a default-OFF switch
would be dead twice over. Arm C is that switch, and it is what makes the 3x
attributable to this emitter rather than to routing.

## Three gates, and gate 2 is fixed

Reaching the emitter requires getting past three independent refusals of the
same method. The middle one used to make arm B unreachable; it no longer does,
and the arm-B/arm-C numbers above are the evidence that it no longer does.

| # | gate | where | opened by |
|---|---|---|---|
| 1 | `string_intrinsic_pin_declines` — the admission conjunction | `jit/src/lib.rs` `try_compile_inner` | `CRATONVM_JIT_NO_STRING_INTRINSIC_PIN=1` |
| 2 | `is_intrinsic_site` → `all_emittable = false` in the invoke-planning loop | `jit/src/lib.rs` ~21962 | **carved out** for the three accessors; blanket restored by `CRATONVM_JIT_NO_IR_STRING_ACCESS_ADMIT=1`, or globally by `CRATONVM_JIT_IR_OVER_INTRINSIC=1` |
| 3 | the expander's own switch | `jit/src/ir.rs` | `CRATONVM_JIT_IR_STRING_INTRINSICS` (default ON) |

Gate 2 carried a bare unconditional `|| cn == "java/lang/String"`. Any method
containing *any* `java/lang/String` invoke set `all_emittable = false`, the
`else` branch left `invoke_info` unset for the whole method, and
`IrBuilder::build`'s `0xb6` arm returned `bail_invoke` on the missing entry
**before** it would offer the site to `try_string_access_intrinsic`. So
`CRATONVM_JIT_NO_STRING_INTRINSIC_PIN=1` alone could not reach the emitter by
any input, and the only arm that could — adding
`CRATONVM_JIT_IR_OVER_INTRINSIC=1` — disables the gate for **every** intrinsic
family at once and therefore prices a different program.

That row now reads

```rust
|| (cn == "java/lang/String" && !ir_string_access_expander_handles(&cn, &mn, &desc))
```

— exactly the three accessors on an unguarded (`java/lang/String`) receiver,
asked through `try_resolve_string_intrinsic` rather than by matching a class
name. Everything else on `java/lang/String` still bails to single-pass, which
is correct: `hashCode`, `equals`, `compareTo` and both `indexOf` forms are
single-pass intrinsics the IR tier would lose.

**This supersedes the previous revision's central correction.** Arm B is one
flag, not two, and `CRATONVM_JIT_IR_OVER_INTRINSIC=1` is no longer part of any
arm on this page. Note also that gate 2 is a *third* thing living in
`try_compile_inner` — like gates 1 and 3's arming, it does not exist at the
other two doors.

## What retirement is gated on

`string_intrinsic_pin_enabled`, `string_pin_fail_closed_enabled`,
`ir_string_access_expander_handles`'s carve-out and
`CRATONVM_JIT_IR_OVER_INTRINSIC` are retirable **only after arm B beats arm A on
a real workload, with `emitted_charAt > 0` witnessing that the emitter ran.**
`CharAtCostCurve` is a probe, not a workload: it is a clean arm precisely
because its only invokes are `String.length()` and `String.charAt(int)`, and on
a real program opening gate 2 may lose intrinsics this probe does not contain.
`java/nio/StringCharBuffer.get()C` — Tomcat's WebSocket text path, and the
method the pin's own 504-vs-135 was measured on — is the obvious next arm.

And the pin cannot be retired on the strength of arm B at all until the door
defect is closed: today the pin governs one door and the population that
motivated it uses another, so "the pin costs nothing" and "the pin is never
asked" are still the same measurement.

## Still open, and separate

**The `char[]` row.** **5.2 ns/char** for a bounds-checked element read in a
compiled counted loop is 44x HotSpot, has nothing to do with `String`, and is
untouched by everything above — including arm B. It is its own question about
baseline codegen quality, and a `charAt` that reached parity with the `char[]`
loop would still be 44x HotSpot. Note that arm B's 59-108 is still **11-20x**
the same VM's own `char[]` loop, so `charAt` has not yet reached even that
ceiling.

**Contradictions found in the tree** (documentation, not behaviour; none in this
page's scope to fix):

* `string_intrinsic_pin_enabled`'s doc claims `grep -rln StringCharAt jit/src/`
  "names exactly two files" and that "`ir_lower.rs` and `ir.rs` mention no
  `JitIntrinsic` variant at all". It names **three** — `ir.rs` compares against
  `JitIntrinsic::StringLength`/`StringIsEmpty`/`StringCharAt`. The comment's
  falsification clause is keyed to `ir_lower.rs` and the emitter landed in
  `ir.rs`, so **the tripwire did not trip itself**.
* `string_intrinsic_pin_census`'s doc says "Nothing prints it yet". That line
  has been added to `jit/src/tiered.rs`; the comment is stale.
* `jit/src/tiered.rs` ~1414 and `string_intrinsic_pin_census`'s doc both quote
  the 326.3/328.6-vs-329.5/333.7 A/B as the motivating null result. That A/B is
  now **explained** (the pin was not asked at the door those methods used) and
  the quoted numbers are pre-branch and pre-emitter — worth a pointer to this
  page's arm table so nobody reads them as current.
* `probes/CharAtWarmShape.java`'s class comment, as noted above.

## Reproducers

`probes/CharAtCostCurve.java`, `probes/CharAtWarmShape.java`,
`probes/CharAtFirstCompile.java`.

```sh
java -cp probes CharAtCostCurve;    cratonvm -cp probes CharAtCostCurve
java -cp probes CharAtWarmShape;    cratonvm -cp probes CharAtWarmShape
java -cp probes CharAtFirstCompile; cratonvm -cp probes CharAtFirstCompile

# arm B — the emitter
CRATONVM_JIT_NO_STRING_INTRINSIC_PIN=1 cratonvm -cp probes CharAtCostCurve
# arm C — its control: same routing, emitter off
CRATONVM_JIT_NO_STRING_INTRINSIC_PIN=1 CRATONVM_JIT_IR_STRING_INTRINSICS=0 \
  cratonvm -cp probes CharAtCostCurve
```

Read a census on **every** timing arm — a timing without an engagement count
cannot distinguish the interesting outcomes here, which is the whole reason this
revision exists:

```sh
# the pin: fired / blind-no-layout / blind-no-resolver / fail-closed
CRATONVM_DBG_JIT_METHOD_STATS=1 <arm> 2>&1 | grep 'String-intrinsic pin'
# the emitter: 10 outcomes, cumulative — tail -1 is the exit census
CRATONVM_DBG_IR_STRING=1 <arm> 2>&1 | grep '\[ir-string\]' | tail -1
# which DOOR compiled it — the reading this page turned on
CRATONVM_DBG=jitc <arm> 2>&1 | grep -E 'bg-compile|OSR-compile|admission'
```

Every flag this page names, in one table:

| flag | token | default | what it does |
|---|---|---|---|
| `CRATONVM_JIT_NO_STRING_INTRINSIC_PIN=1` | `-string-intrinsic-pin` | pin ON | opens gate 1 — **this alone is arm B** |
| `CRATONVM_JIT_IR_STRING_INTRINSICS=0` | `-ir-string-intrinsics` | emitter ON | closes gate 3 — with the flag above, arm C |
| `CRATONVM_JIT_NO_IR_STRING_ACCESS_ADMIT=1` | `-ir-string-access-admit` | carve-out ON | restores gate 2's blanket `java/lang/String` bail |
| `CRATONVM_JIT_IR_OVER_INTRINSIC=1` | `ir-over-intrinsic` | off | disables gate 2 for **every** intrinsic family |
| `CRATONVM_JIT_NO_STRING_PIN_FAIL_CLOSED=1` | `-string-pin-fail-closed` | fail-closed ON | reverts the resolver-blind fail-closed rule |
| `CRATONVM_DBG_JIT_METHOD_STATS=1` | `CRATONVM_DBG=jit-method-stats` | off | the pin census |
| `CRATONVM_DBG_IR_STRING=1` | `CRATONVM_DBG=ir-string` | off | the emitter census |
| `CRATONVM_DBG_JITC=1` | `CRATONVM_DBG=jitc` | off | the admission verdict, and which door compiled |
