# `String.charAt` costs a flat 186-196 ns/char, and the fix for it is written but NOT MEASURED

**Status:** OPEN.

* **Measured** 2026-09-01 on `dev` @ `56d6c3722`, x86-64 Linux, 8 cores,
  `/proc/loadavg` 5-11 recorded per run — the effect is 40-370x, two orders
  outside that band. Real-JDK mode, JDK 25, default flags.
* **Implemented** 2026-09-01 on `claude/audit-impl-20260901`: all three of this
  page's original recommendations landed as code.
* **NOT MEASURED.** Nothing on that branch has been built or run. Every number
  on this page is still the pre-change number. "The emitter exists" is not "the
  cliff is fixed" — see [The A/B that has not been run](#the-ab-that-has-not-been-run),
  which also corrects the A/B the new emitter's own section header names:
  that pair of flags does not reach it.

## The measurement

`probes/CharAtCostCurve.java`: three loop bodies at growing inner rep counts,
over a 100,000-char `String` and the `char[]` from the same text.

| body | reps | CratonVM | HotSpot 25 |
|---|---:|---:|---:|
| `s.charAt(i)` | 2 | 190.0 ns/char | 41.6 |
| | 10 | 189.7 | 0.75 |
| | 50 | 227.5 | 0.50 |
| | 200 | 186.1 | 0.52 |
| | 200 | 194.8 | 0.56 |
| | 200 | 191.3 | 0.53 |
| | 1000 | 195.6 | 0.49 |
| `charAt`, `length()` hoisted | 200 | 155.8 | 0.47 |
| | 1000 | 177.1 | 0.46 |
| `a[i]` on a `char[]` | 200 | **5.38** | 0.12 |
| | 1000 | **5.22** | 0.12 |

Two things to read off it:

* **It is flat.** From 200,000 characters to 100,000,000 the cost never moves.
  There is no warm-up ramp — whatever body runs at 200,000 characters is still
  running at 100 million.
* **It is `charAt`, not the loop.** The identical loop over a `char[]` is
  **36x cheaper in the same VM**. Not bounds checks, not the induction
  variable, not the compare. The call.

Against HotSpot: `charAt` **370x**, `char[]` **44x**.

## A 3 ns path exists in the same binary

`probes/CharAtWarmShape.java` runs a byte-identical body and prints:

```
CratonVM   warm3x50    3.44, 2.96 ns/char
           warm3x200   3.31, 3.46 ns/char
HotSpot    warm3x50    0.25       warm3x200  1.24
```

**~90x faster than the table above, same binary, same class file, same method
body, same host.** So the inline `charAt` lowering does engage —
`jit/src/x64/bytecode_walk.rs` emits a coder-branch decode with no `CALL`, and
this is it running. The two probes differ only in how the method is reached.

(The probe's own class comment says dropping the warm reps to 50 "loses the fast
body entirely (measured 340 ns/char)", which contradicts the `warm3x50` row
above. Its two arms are *separate methods* — `scanSmall` and `scanBig` — so the
comment is describing a run that is not the one recorded here. Unreconciled;
believe the table, not the comment, until someone re-runs it.)

## Five hypotheses, five refutations

Every plausible discriminator was tested and each is ruled out by its own
measurement. This section is the value of the page: it is the list nobody
should pay for twice. **All five are still true and none of the 2026-09-01
changes touches them.**

| hypothesis | test | result |
|---|---|---|
| OSR entry is slower than method entry | one long call vs 2,000 short ones (`OsrVsEntry`) | 239 vs 205 ns/char — **no** |
| the callee was cold when the caller compiled | make `charAt` hot in a third method first, then run | 327 vs 291 ns/char — **no** |
| the caller's shape decides it | six callers: direct, static helper, `LongSupplier`, user-declared interface | 312-343 ns/char, all six — **no** |
| the first compile is final, and its context decides | two identical methods, one first entered from `main`, one from a helper, then interleaved reruns (`probes/CharAtFirstCompile.java`) | 278-324 ns/char for both, all eight rows — **no** |
| it is a scale threshold | 200k → 100M characters in one call | flat 186-196 — **no** |

So: a body 90x faster exists, is selected by something, and that something is
none of tiering door, warm order, caller kind, first-compile context or scale.

## Neither documented lever moved it

```
default                                  329.5, 333.7 ns/char
CRATONVM_JIT_NO_STRING_INTRINSIC_PIN=1   326.3, 328.6
CRATONVM_JIT_IR_OVER_INTRINSIC=1         333.4, 366.3
```

`string_intrinsic_pin_enabled` is default-ON and its own comment carries the
measurement that motivated it:

> ```
> C2/IR body, intrinsic dropped   504 ns/call
> C1 body, intrinsic emitted      135 ns/call   (3.7x)
> ```
>
> …the delta is that large because the intrinsic does not replace one call
> with one instruction — it replaces the whole
> `charAt → isLatin1 → StringLatin1.charAt → String.checkIndex →
> Preconditions.checkIndex` chain, whose tail is a registered NATIVE.

Right diagnosis, right defect. What the numbers said is that the pin was **not
reaching this population**: switching it off cost nothing, so it was not on.
A null result of that shape is exactly what a gate that never engages looks
like from outside, and **no timing can separate "it fired and did not help"
from "it never fired"** — which is what the census in
[What changed](#what-changed-on-2026-09-01) now exists to settle.

## What changed on 2026-09-01

Four items, all on `claude/audit-impl-20260901`, all **written and reviewed
against the code, none of them built or run**.

### 1. The printed verdict now contains the pin — recommendation 1, DONE

`string_intrinsic_pin_verdict` (`jit/src/lib.rs`) is a pure classifier called by
**both** the printed verdict chain and the real eligibility conjunction, so the
reason a run reports and the decision a run takes cannot drift apart again. It
distinguishes four states:

| verdict | meaning | admission line |
|---|---|---|
| `Pinned` | site present, pin declined the optimizing tier | its own refusal arm |
| `DisabledByFlag` | site present, `CRATONVM_JIT_NO_STRING_INTRINSIC_PIN=1` | admitted **+ note** |
| `BlindNoLayout` | a `String`/`CharSequence`-declared site, but no `StringFieldLayout` resolved at this door | admitted **+ note** |
| `BlindNoResolver` | candidate sites, and no constant-pool invoke resolver at this door | admitted **+ note** |

The last two are the silent failures the whole investigation turned on. The
three non-refusing states are **appended in parentheses** to the admission line
rather than replacing it, so `metrics::CompilationReport::admission` keeps
meaning "admitted", which is what its consumers read it as.

`BlindNoLayout` is deliberately narrowed to methods that call something declared
on `java/lang/String` or `java/lang/CharSequence` — otherwise it would print
against essentially every method in the program, which is how a diagnostic
becomes noise instead of evidence.

### 2. The pin fails closed in ONE of the two blind cases, deliberately

The asymmetry is the interesting part, and it was checked rather than assumed.

**Layout `None` → still fails OPEN, but is counted.** `resolved_string_layout`
is resolved once per compilation and the same value is threaded to this gate, to
the single-pass registration loop's `try_resolve_string_intrinsic`, and to
`x64::compile`'s `compiler.string_layout`. Verified: `try_resolve_string_intrinsic`
tests the class name first and only then reaches `let layout = string_layout?;`,
so with `None` it returns `None` for *every* site and the single-pass backend
emits no inline decode either. Pinning would buy a C1 body with no intrinsic in
place of a C2 body — a pure downgrade. It is still counted, because "the layout
did not resolve" is the leading untested hypothesis for the whole `charAt`
population.

**Resolver `None` → now fails CLOSED**, and only when a layout *did* resolve.
That is a missing input at one door, not evidence about the method. The cost was
checked, not assumed: with `scan.invoke_ops` non-empty and no resolver the
single-pass path takes `jitc_bail!("cp_invoke_resolver")` (`jit/src/lib.rs`),
which does **not** set `backend_attempted`, so the method is not permanently
bail-listed — worst case is one abandoned compile attempt, retried later. The
asymmetry matters because **a compiled body is installed and kept**: an
intrinsic-less body made at a blind door is not a slow first attempt, it is the
body for the life of the process.

Kill switch: `CRATONVM_JIT_NO_STRING_PIN_FAIL_CLOSED=1`
(`CRATONVM_JIT=string-pin-fail-closed`, default-ON opt-out). The rule is
subordinate to the pin — switching the pin off switches the fail-closed rule off
with it, or the two A/B arms would differ by more than one thing.

### 3. Four counters, and a zero that is readable — recommendation 2, DONE

`string_intrinsic_pin_census() -> (fired, blind-no-layout, blind-no-resolver, fail-closed)`,
printed by `dump_method_stats_to_stderr` (`jit/src/tiered.rs`) as

```
[cratonvm] JIT String-intrinsic pin: fired=N blind-no-layout=N blind-no-resolver=N fail-closed=N
```

under `CRATONVM_DBG_JIT_METHOD_STATS=1` (or `CRATONVM_DBG=jit-method-stats`).
`fired` exists so that a **zero is readable**: the 326.3/328.6-vs-329.5/333.7
A/B above is precisely what a pin that never engages looks like from outside.

Two caveats about this instrument, both from the code:

* The line sits **after** the `DIAG_CORE` early return in
  `dump_method_stats_to_stderr`. A run that never built a tiered manager prints
  no line at all — absence of the line is not `fired=0`.
* `blind-no-resolver` is **predicted to read zero**, and that is written down in
  the code as a falsifiable claim: all three production doors
  (`vm/src/runtime/interpreter/jit_bridge.rs`, lines ~5871, ~6090, ~7659) pass
  `Some(&invoke_resolver)` — verified. A non-zero reading would name a door
  nobody knew existed. All three also pass `Some(&string_layout_resolver)`, so a
  non-zero `blind-no-layout` is a fact about what that **resolver returns**
  (String not yet loaded, no `coder` field), never about a door that forgot it.

### 4. The IR tier has a String-access emitter — recommendation 3, DONE

`jit/src/ir.rs` gained `length()`, `isEmpty()` and `charAt(int)`, expanded at
IR-build time into **existing primitive nodes** — no new `Op`.

Shape (a), new `Op::StringCharAt` variants lowered in `ir_lower`, was blocked by
evidence rather than by taste: `jit/src/ir_verify.rs`'s `expected_arity` is an
exhaustive `match` over `Op` ending at `Op::Dead => (0, 0)` with **no wildcard
arm**, so a new variant is a compile error in a file the change could not touch.
Shape (b) is also better on merit: `value` and `coder` become ordinary
`Op::Load`s the optimizer can see, and neither field is written after
construction, so `ir_optimize`'s LICM may hoist both out of a counted loop and
leave only the element read in the body — which is the shape HotSpot's 0.5
ns/char comes from, and which is unreachable for any node the tier treats as one
opaque unit. **Whether it does hoist them is a measurement nobody has taken.**

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
  `Op::Merge` at a pc that is not a branch target, which the merge bookkeeping is
  not built to see;
* `try_resolve_string_intrinsic` is **called, not re-derived**, so the
  receiver-guard policy stays the single-pass backend's own;
* but the IR tier then **additionally refuses the guarded (`CharSequence`) case**
  — no IR node reads an `ObjectHeader` class id, and the only substitute,
  `Op::InstanceOf`, is an opaque helper `CALL` that would sit in the loop body.
  Emitting the decode unguarded would admit a `StringBuilder` receiver to a
  String-layout decode: a wrong **value**, not a slow one. Counted as
  `guarded_site_refused`, and the `invokeinterface` (`0xb9`) arm calls the
  expander *only* to count that population;
* narrow-oop safety comes from **never emitting a displacement**: the expansion
  reads only `value_field_index`, `coder_field_index`, `has_coder` and
  `string_class_id` — slot indices and identities, never a byte offset — so it
  cannot encode a width. (The byte offsets in `StringFieldLayout` are *not*
  process-stable: the compact arm falls back to legacy addresses before a
  `CompactLayout` is registered. That is why publishing the layout once is sound
  for these four fields and for no others.)
* `coder` is loaded **before** `value`, because the `Op::Load` helper arm is a
  `CALL` and loading the `Ref` last is the shortest window with a live oop across
  one;
* every uncertain case **deopts, none throws**: null receiver, null `value` and
  out-of-range index all reach `Op::Guard { bci }` at the invoke's own bci, where
  `IrBuilder::build`'s snapshot still holds receiver (+index) on the operand
  stack. The interpreter re-executes the `invokevirtual` and the native raises
  the exact `NullPointerException` / `StringIndexOutOfBoundsException`;
* the expansion **refuses inside an open splice** (`in_splice_refused`), because
  a spliced region pushes no snapshot — the exact shape behind the prior defect
  that turned an `IndexOutOfBoundsException` into an `InternalError`
  (`ir-inline-turns-an-index-out-of-bounds-into-an-internalerror-FIXED-20260828`).

Census: `ir_string_intrinsic_census()` over 10 outcomes — `sites_seen`,
`flag_off`, `no_layout`, `not_an_accessor`, `guarded_site_refused`,
`in_splice_refused`, `shape_refused`, `emitted_length`, `emitted_isEmpty`,
`emitted_charAt`. `no_layout` exists specifically so that "the emitter is
unwired" is not indistinguishable from "the emitter found nothing".

**How to read it:** nothing in `vm-cli` calls `ir_string_intrinsic_census` —
verified, it has no caller outside `jit/src/ir.rs`. The only way to read it is
the per-decision line under `CRATONVM_DBG_IR_STRING=1`, which is cumulative, so
`tail -1` of the run's `[ir-string]` lines IS the exit census.

The orchestrator wiring is present: `jit/src/lib.rs`'s `try_compile_inner` calls
`ir::publish_string_layout(layout)` immediately after resolving
`resolved_string_layout`. **Verified — without that line the emitter is inert
and would report `no_layout=N, emitted_charAt=0`.**

Kill switch: `CRATONVM_JIT_IR_STRING_INTRINSICS=0`
(`CRATONVM_JIT=ir-string-intrinsics`), **default ON** — because the pin
currently keeps these methods off the IR tier entirely, so a default-OFF switch
would be dead twice over.

## The A/B that has not been run

**While the pin is on, none of the IR emitter is reached.** That much was known.
What was *not* known, and is the single most important correction on this page:

### There are THREE gates, not two, and the second one ignores the pin flag

Reading the emitter requires getting past all three:

| # | gate | where | opened by |
|---|---|---|---|
| 1 | `string_intrinsic_pin_declines` — the admission conjunction | `jit/src/lib.rs` `try_compile_inner` | `CRATONVM_JIT_NO_STRING_INTRINSIC_PIN=1` |
| 2 | `is_intrinsic_site` → `all_emittable = false` in the invoke-planning loop | `jit/src/lib.rs` (~21770-21845) | `CRATONVM_JIT_IR_OVER_INTRINSIC=1` |
| 3 | the expander's own switch | `jit/src/ir.rs` | `CRATONVM_JIT_IR_STRING_INTRINSICS` (default ON) |

Gate 2 is the one nobody accounted for. Its predicate contains a bare

```rust
|| cn == "java/lang/String"
```

— an unconditional class-name test that does **not** consult
`string_intrinsic_pin_enabled` at all. Any method containing *any*
`java/lang/String` invoke sets `all_emittable = false`, and the `else` branch
then leaves `invoke_info` **unset for the whole method**. `IrBuilder::build`'s
`0xb6` arm reads `self.invoke_info.get(&pc)` and returns `bail_invoke` on a miss
— **before** it would call `try_string_access_intrinsic`. So the method bails to
single-pass and the expander is never entered.

Consequence: `CRATONVM_JIT_NO_STRING_INTRINSIC_PIN=1` **alone does not reach the
new emitter.** The arm that does is both flags together. This is the same
three-doors shape this JIT has been caught by before (the `iinc_w` scan refusal,
the `checkcast` OSR door, the thin-native bind): a lever that engages at one
level and is reasoned about at another.

For `CharAtCostCurve.scan` this is a clean arm rather than a risky one: its only
invokes are `String.length()` and `String.charAt(int)`, and the expander handles
both, so opening gate 2 loses no other intrinsic in that method. On a real
workload it may well lose others, and that is a separate measurement.

### The settling measurement

Three timing arms, one binary. Read the two censuses on **every** arm — a timing
without an engagement count cannot distinguish the interesting outcomes here.

```sh
# Arm A — default. Pin ON; gates 1 and 2 both shut; the emitter is unreachable.
#         This is the currently-measured 186-196 ns/char shape.
cratonvm -cp probes CharAtCostCurve

# Arm B — the arm everything is gated on. All three gates open.
CRATONVM_JIT_NO_STRING_INTRINSIC_PIN=1 CRATONVM_JIT_IR_OVER_INTRINSIC=1 \
  cratonvm -cp probes CharAtCostCurve

# Arm C — the control for arm B: same routing, emitter OFF. Arm C is what
#         arm B was before the emitter existed, so B-minus-C is the emitter.
CRATONVM_JIT_NO_STRING_INTRINSIC_PIN=1 CRATONVM_JIT_IR_OVER_INTRINSIC=1 \
CRATONVM_JIT_IR_STRING_INTRINSICS=0 \
  cratonvm -cp probes CharAtCostCurve

# Arm B' — diagnostic, NOT a control: pin off but gate 2 still shut. Predicted
#          to look exactly like arm A. If it does not, gate 2 is not what this
#          page says it is and everything above needs re-deriving.
CRATONVM_JIT_NO_STRING_INTRINSIC_PIN=1 cratonvm -cp probes CharAtCostCurve
```

The two censuses, per arm:

```sh
# the pin: fired / blind-no-layout / blind-no-resolver / fail-closed
CRATONVM_DBG_JIT_METHOD_STATS=1 <arm> 2>&1 | grep 'String-intrinsic pin'

# the emitter: 10 outcomes, cumulative — tail -1 is the exit census
CRATONVM_DBG_IR_STRING=1 <arm> 2>&1 | grep '\[ir-string\]' | tail -1

# the per-method admission verdict, which now names the pin
CRATONVM_DBG_JITC=1 <arm> 2>&1 | grep admission
```

Then repeat all of it for `CharAtWarmShape` and `CharAtFirstCompile` — the
former because it is the only probe known to reach the 3 ns body and so the only
one that can say whether the emitter matches it, the latter because it is the
eight-row control against first-compile context.

**Predictions, recorded before the run so they are falsifiable:**

* Arm A, `fired`: if `> 0` the pin engages and the 190 ns body *is* the
  single-pass body, which refutes the whole "the pin never fired" reading and
  makes the intrinsic itself the suspect. If `= 0`, the next number to read is
  `blind-no-layout`.
* `blind-no-resolver = 0` on every arm (all three doors pass a resolver).
  Non-zero names an unknown door.
* Arm B, `[ir-string]`: `sites_seen > 0` **and** `emitted_charAt > 0` are
  required before the arm-B timing means anything at all. `no_layout=N,
  emitted_charAt=0` means the layout publish did not happen on that run;
  `sites_seen=0` means gate 2 is still shut and the arm is not the arm.
* `guarded_site_refused = 0` on these probes — every site is declared on
  `java/lang/String`, not `CharSequence`.

### What retirement is gated on

`string_intrinsic_pin_enabled`, `string_pin_fail_closed_enabled` and
`CRATONVM_JIT_IR_OVER_INTRINSIC` are retirable **only after arm B beats arm A on
a real workload, with `emitted_charAt > 0` witnessing that the emitter ran** —
not on the code existing. `ir_over_intrinsic_enabled`'s own doc comment already
stages that retirement, and `string_intrinsic_pin_enabled`'s comment states the
tripwire grep that is supposed to trigger it.

Do **not** read this page as saying the `charAt` cliff is fixed. It says the
instrument that could not name the decision now exists, and that a plausible fix
is in the tree unmeasured.

## Still open, and separate

**The `char[]` row.** **5.2 ns/char** for a bounds-checked element read in a
compiled counted loop is 44x HotSpot, has nothing to do with `String`, and is
untouched by everything above. It is its own question about baseline codegen
quality, and a `charAt` that reached parity with the `char[]` loop would still
be 44x HotSpot.

**Contradictions found in the tree while writing this revision** (all
documentation, none of them behaviour, none in this page's scope to fix):

* `string_intrinsic_pin_enabled`'s doc claims `grep -rln StringCharAt jit/src/`
  "names exactly two files" and that "`ir_lower.rs` and `ir.rs` mention no
  `JitIntrinsic` variant at all". It now names **three** — `ir.rs` compares
  against `JitIntrinsic::StringLength`/`StringIsEmpty`/`StringCharAt`. The
  comment's own falsification clause is keyed to `ir_lower.rs`, and the emitter
  landed in `ir.rs`, so **the tripwire did not trip itself**.
* `string_intrinsic_pin_census`'s doc says "Nothing prints it yet — the JIT
  stats dump lives in `jit/src/tiered.rs` — so until a line is added there it is
  read through this accessor." That line **has** been added; the comment is
  stale.
* `probes/CharAtWarmShape.java`'s class comment, as noted above.

## Reproducers

`probes/CharAtCostCurve.java`, `probes/CharAtWarmShape.java`,
`probes/CharAtFirstCompile.java`.

```sh
java -cp probes CharAtCostCurve;    cratonvm -cp probes CharAtCostCurve
java -cp probes CharAtWarmShape;    cratonvm -cp probes CharAtWarmShape
java -cp probes CharAtFirstCompile; cratonvm -cp probes CharAtFirstCompile
```

Every flag this page names, in one table:

| flag | token | default | what it opens |
|---|---|---|---|
| `CRATONVM_JIT_NO_STRING_INTRINSIC_PIN=1` | `-string-intrinsic-pin` | pin ON | gate 1 |
| `CRATONVM_JIT_IR_OVER_INTRINSIC=1` | `ir-over-intrinsic` | off | gate 2 |
| `CRATONVM_JIT_IR_STRING_INTRINSICS=0` | `-ir-string-intrinsics` | emitter ON | closes gate 3 |
| `CRATONVM_JIT_NO_STRING_PIN_FAIL_CLOSED=1` | `-string-pin-fail-closed` | fail-closed ON | reverts item 2 |
| `CRATONVM_DBG_JIT_METHOD_STATS=1` | `CRATONVM_DBG=jit-method-stats` | off | the pin census |
| `CRATONVM_DBG_IR_STRING=1` | `CRATONVM_DBG=ir-string` | off | the emitter census |
| `CRATONVM_DBG_JITC=1` | — | off | the per-method admission verdict |
