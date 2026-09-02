# `String.charAt` — FIXED 2026-09-02. `String` is `final`, and that is what killed its intrinsic

**Was `docs/known-issues/perf/string-charat-loop-cost-and-the-unsteerable-intrinsic-20260901.md`,
opened 2026-09-01 as "OPEN. Measured. The fix is real, it is ~3x, and it does
not close the gap."**

The page's title was right and its subject was one level deeper than it
reached. `charAt` in a compiled counted loop cost ~340-400 ns/char while a
**byte-identical body elsewhere in the same binary cost 3**, and no documented
lever moved it. The page tested five hypotheses about the METHOD, refuted all
five, correctly identified the discriminator as the compile DOOR — and stopped
one question short of why the doors differ.

They differ because **`java/lang/String` is `final`**.

`invokevirtual_site_final_owner` (default-ON since 2026-08-28) therefore
answers for every `String.charAt`/`length`/`isEmpty`/`hashCode` site in the
tree, and `try_compile_inner`'s invoke loop rewrites `invoke_kind` 0 → 1 on
that answer. That is a correct claim about **dispatch** and a disastrous one
about **codegen**: the instance call-site intrinsic gate is
`invoke_kind == 0 || invoke_kind == 2`, and a kind-1 site enters the
inline/direct-bind ladder first and leaves the loop through its `continue`. So
the site was bound to a real `CALL` into
`String.charAt → isLatin1 → StringLatin1.charAt → checkIndex →
Preconditions.checkIndex` and never offered the inline decode.

Silently. Not declined, not blind, not counted: all three `string-intrinsic`
diagnostics — `MISSED`, `DECLINED`, `SKIPPED-GATE` — sit **past** the point the
site left, so they printed nothing while the intrinsic was completely dead at
this door. The pin's four counters could not see it either; `fired=2` was a
true statement about a decision that no longer had anything to decide.

The OSR door runs no such rewrite. That is the whole of the 100x.

**Second pass, same day.** This page retired with three residuals listed
under *Still open*. Two are now closed and the third is re-measured; the
work is in [What the second pass
closed](#what-the-second-pass-closed-2026-09-02). The short version: the IR
tier's String expander went from **78 to 3.4 ns/char** (it now ties the
single-pass body rather than being 23x behind it), and the enumeration this
page asked for turned up **two more final classes** whose call sites the same
rewrite was taking — `Integer.intValue` at 76 ns/op and `Long.longValue` at
**424**, both now ~1.6.

## Measured

`probes/CharAtCostCurve.java`, the page's own witness, `charAt` rows,
steady-state reps, two interleaved rounds, `/proc/loadavg` recorded per round:

| arm | ns/char | vs HotSpot |
|---|---:|---:|
| HotSpot 25, same host, same run | **0.23** | 1x |
| before (dev @ `a9acf3ec2`) | ~400 | ~1700x |
| **after** | **~3.3** | **~14x** |
| after, `CRATONVM_JIT_NO_DEVIRT_INTRINSIC_YIELD=1` | ~560 | — |
| after, `CRATONVM_JIT_NO_STRING_INTRINSIC_PIN=1` (the page's "arm B") | ~100 | — |

The third row is the A/B: **one binary, one flag, ~170x**. The `before` row is
a different binary *and* a different link profile (see "A note on the build"
below), so the honest number is the within-binary one.

And `probes/CharAtDoorProbe.java`, added here — five byte-identical bodies in
ONE class, ONE run, on a quiet host (load 3.26):

| arm | caller | yield ON | yield OFF |
|---|---|---:|---:|
| `scanDirect` | straight from `main` → method-entry door | **1.88** | **262.70** |
| `scanSmallFirst` | functional interface → OSR door | 2.05 | 1.87 |
| `scanBigFirst` | " | 2.15 | 1.91 |
| `scanRamp` | " | 2.39 | 1.79 |
| `scanBigThenSmall` | " | 2.45 | 1.75 |

**140x on the affected arm and the four unaffected arms unchanged** — which is
the shape a targeted fix has, and the shape a broad one does not.

Engagement, so a zero would have been readable:
`[cratonvm] JIT devirt yielded to intrinsic: 2` — the two sites in `scanDirect`.

## How the page got to one question short

Every step it took was sound. Worth recording because the same shape will
recur:

* **The five hypotheses were all about the method** — OSR-vs-entry as a
  property of the method, callee warm order, six caller shapes, first-compile
  context, scale within one call — and each was refuted by its own measurement.
  Correct, and none of them could have found this.
* **"The discriminator is the door" was right**, and the page then reasoned
  about what the doors DO differently in the machinery it had been reading —
  the pin, the IR expander's arming, gate 2 — all of which live in
  `try_compile_inner`. It concluded the OSR door was the DEPRIVED one. It is
  the other way round: the OSR door is the only one that still emits the
  intrinsic.
* **The instruments agreed with the wrong conclusion** because they were all
  installed downstream of the branch that had already taken the site away.
  `fired=0` on `CharAtWarmShape` was read as "this door never asks"; the
  matching `fired=2` on `CharAtCostCurve` was read as "this door asks and the
  pin works". Both readings were true and neither was load-bearing.

What broke it open was not more reading. It was one probe that puts both caller
shapes in one class (`CharAtDoorProbe`), and then one existing flag:
`CRATONVM_JIT_FINAL_DEVIRT=0` moved the slow arm from 349.64 to 6.85 ns/char on
an unmodified binary. **51x from a flag that names the cause**, before a line of
code was written.

## What was fixed

Two commits on `perf/charat-cliff-20260902`.

**The rewrite yields to the intrinsic.** A site that
`try_resolve_intrinsic` (layout-independent) or `try_resolve_string_intrinsic`
(layout-aware) would take is left at `invoke_kind == 0` so the gate can still
claim it. The predicate is `site_yields_to_call_site_intrinsic`, extracted so it
can be asserted rather than reasoned about.

The blast radius is narrower than it looks, and that is the point. The caller
only consults it where `cp_invokespecial_owner_resolver` would otherwise have
answered — a **private** target, or a **final** one. No intrinsic matches a
private method, so the JVMS 5.4.6 correctness rule keeps every site it had
(`String.isLatin1`, `coder`, `checkIndex` — the very chain the missing
intrinsic sends the program down — are all still pinned, and a test asserts
it). What is left is exactly "a final class with an instance intrinsic".
`java/lang/String` is the measured member of that set; any other is the same
defect by construction and is not measured here.

Counted (`DEVIRT_YIELDED_TO_INTRINSIC`), printed beside the pin's census, and
`CRATONVM_JIT_NO_DEVIRT_INTRINSIC_YIELD=1` restores the old ordering as the B
arm. Five tests pin the predicate, including the kill switch.

## The page's other open questions, answered

### "What retirement is gated on" — answered, and it argues the other way

> *"…retirable **only after arm B beats arm A on a real workload**, with
> `emitted_charAt > 0` witnessing that the emitter ran."*

Arm B does not beat arm A. It loses by **30x**: ~100 ns/char against ~3.3, on
the fixed binary, same run. The page measured arm B at 3-5x *better* than arm A
because arm A had no String intrinsic in it at all — both arms of that
comparison were pricing a program the pin was no longer protecting.

So `string_intrinsic_pin_enabled` and `string_pin_fail_closed_enabled` are
**not** retirable, and the evidence for keeping them is now direct rather than
inherited: with the intrinsic actually reaching the emitter, pinning a String
accessor method to the single-pass backend is worth 30x. The 504-vs-135 ns/call
reading the pin was built on is confirmed, not superseded.

What that leaves questionable is the opposite half — gate 2's carve-out and
`CRATONVM_JIT_IR_OVER_INTRINSIC`, which exist to let the IR expander reach a
population that measurably should not have it. They are not retired here
(nothing on this branch measured a workload where the expander wins), but they
are now the side carrying the burden of proof.

### "Arm B rises with scale" / "is LICM hoisting `value` and `coder`?" — answered: no, and why

The page named the next measurement and it has been made.
`CRATONVM_DBG_IR_STRING=1` with the pin off gives the engagement:

```
[ir-string] sites_seen=4 flag_off=0 no_layout=0 not_an_accessor=0
            guarded_site_refused=0 in_splice_refused=0 shape_refused=0
            emitted_length=2 emitted_isEmpty=0 emitted_charAt=2
```

The expander runs on every site. And `CRATONVM_DBG_LICM=1` on the same run:

```
[DBG_LICM] header N: body N node(s), N load(s), hard_barrier=true
```

on **every** candidate header. So the emitter's design note — *"the win depends
on `ir_optimize` hoisting those two `Op::Load`s out of the counted loop"*, and
*"whether it does is a measurement nobody has taken"* — rests on something that
does not happen. `loop_has_hard_barrier` refuses a loop containing any node
that is not pure, not control and not `Load`/`Store`/`Phi`, and the charAt
expansion's own `Op::Guard`s, `Op::ArrayLoad`s and `Op::ArrayLength` are exactly
that. **The expansion disqualifies its own LICM.**

That is why arm B is ~100 ns/char and not ~3: per character it does two field
loads, an `arraylength`, two array loads and four guards, none of which leave
the loop.

**Closed in the second pass** — `licm-read-hoist`, below.

### The four documentation contradictions — all four closed

* `string_intrinsic_pin_enabled`'s "names exactly two files" grep — already
  corrected on dev (`19812fecc`); the doc now shows the three-file reading and
  explains the re-key.
* `string_intrinsic_pin_census`'s "Nothing prints it yet" — already corrected
  on dev; the line is in `tiered.rs`.
* The 326.3/328.6-vs-329.5/333.7 A/B, quoted at `STRING_PIN_FIRED` and in
  `tiered.rs`'s census note as the motivating null result — **corrected here,
  and it was worse than stale**: both arms of that A/B measured a program with
  no String intrinsic in it, because the devirtualisation had already taken
  every site. Both citations now say so.
* `probes/CharAtWarmShape.java`'s class comment ("dropping the warm reps to 50
  loses the fast body entirely (measured 340 ns/char)") against its own
  `warm3x50` row of 3.22-3.44 — **corrected here**. Both readings were real and
  the rep count was never the variable: which door compiles the body is a race
  between the invocation counter and the back-edge counter, and only the
  method-entry door ran the rewrite that cost the intrinsic. Since the fix the
  two doors emit the same decode, so the race no longer decides 100x.

### The door gap itself — closed on dev, separately

`86d840a87` made all three doors ask the pin through the `CompileAdmission`
token, and its own commit message is careful that the decision is **vacuous**
at the two direct doors because neither has a route to the optimizing tier —
"the whole value is the counter". That is still true, and the counter is what
prints the per-door census quoted above. It did not fix this defect and did not
claim to.

## What the second pass closed (2026-09-02)

Three residuals were listed here. Two are closed by code and the third is
re-measured and reattributed. All numbers below are one binary, x86-64 Linux,
`lto=off`, three ABBA-interleaved rounds, n as marked.

### 1. `licm-read-hoist` — the expander's loads are hoisted. 78 → 3.4 ns/char

Exactly the fix this page named. `ir_optimize` gained `loop_writes_memory`,
the same question as `loop_has_hard_barrier` asked about WRITES only:
`Op::ArrayLoad` and `Op::ArrayLength` are reads and cannot clobber a hoisted
load, and `Op::Guard` carries no memory edge at all. `Op::Store` is NOT exempt
there — the new arm does no alias analysis, so it wants a body that writes
nothing rather than one whose writes it would have to reason about.

The hoist itself is an ADDITIVE arm beside the `Op::ArrayLength` one, on the
same three terms, and the existing general load hoist is untouched:

1. **the value cannot change** — nothing in the body writes, and the base is
   loop-invariant;
2. **the throw point does not move** — an `Op::Load` deopts rather than faults
   on a null base, and a deopt raised from the pre-header would rebuild an
   interpreter frame at a bci the loop never reached. So the base must already
   be dereferenced on entry to the header: either the load is anchored there
   (the `ArrayLength` arm's own condition) or some other header-anchored
   load/`arraylength` reads the same base. For a counted
   `for (i = 0; i < s.length(); i++)` that is exactly true — `s.length()`
   expands at the header off the same receiver `s.charAt(i)` uses;
3. **the memory token is re-anchored** — without it the hoist is inert
   whenever the body threads memory through a read.

Placed ABOVE the pre-header guard, like the `ArrayLength` arm and for its
reason: for a nested inner loop `body` over-approximates and drags the
enclosing pre-header in with it. Over-approximation is the safe direction for
every test in the arm — a bigger body makes `is_loop_invariant` stricter and
the header-deref set smaller, so it can only refuse a hoist.

`CRATONVM_DBG=licm` on the arm this exists for now reads

```
[DBG_LICM] header 7: body 30 node(s), 4 load(s), hard_barrier=true writes_memory=false
[DBG_LICM] read-hoist load 46 (inputs [42, 45, 3, 13]): HOIST to preheader 22, mem 45 -> 14
[DBG_LICM] read-hoist load 34 (inputs [7, 33, 3, 13]): HOIST to preheader 22, mem 33 -> 14
[DBG_LICM] read-hoist load 45 (inputs [42, 34, 3, 32]): HOIST to preheader 22, mem 34 -> 14
[DBG_LICM] read-hoist load 33 (inputs [7, 23, 3, 32]): HOIST to preheader 22, mem 23 -> 14
[DBG_LICM] hoisted 4 invariant load(s) to loop pre-header(s)
```

### 2. The expander's two loads were `jit_getfield` helper CALLs

Found on the way to (1) and worth its own row, because it is a different
defect with a different blast radius. `try_string_access_intrinsic` emits its
`coder` and `value` reads at the **invoke** pc, and `ir_lower`'s inline
compact-`getfield` fast path is keyed by **getfield** pc — so neither load had
a row and both fell back to the checked helper. **917,203,334** helper CALLs
in one `probes/CharAtCostCurve.java` run, against `IR-tier inline-getfield
refusals: no-compact-slot-for-pc=8`.

The compact-field map is now keyed by `(pc, is_reference)` — one pc can
legitimately own two rows, which is what this expansion needs — and
`try_compile_inner` installs the two rows from the layout THIS compile
resolved, never from `ir::published_string_layout()`, whose `OnceLock` may
carry offsets from before `java/lang/String` had a `CompactLayout`. Refused
for a legacy `char[]` String, a narrow compact `value`, or a negative body
offset.

### The arm-B table, with each fix isolated

`probes/CharAtCostCurve.java`, `charAt` rows, pin OFF (the IR tier) unless
stated. n=9-18 at reps=200, n=3-6 elsewhere.

| reps | HotSpot | default (pin ON) | neither fix | rows only | hoist only | **both** |
|---:|---:|---:|---:|---:|---:|---:|
| 2 | 60.3 | 3.32 | 82.6 | 16.1 | 3.40 | **3.22** |
| 10 | 3.26 | 3.33 | 77.0 | 15.6 | 3.55 | **3.19** |
| 50 | 0.52 | 3.69 | 79.7 | 16.3 | 3.48 | **3.24** |
| 200 | 0.47 | 3.39 | 76.2 | 15.8 | 3.54 | **3.45** |
| 1000 | 0.47 | 3.40 | 77.6 | 15.3 | 3.63 | **3.52** |

* **The hoist is the fix**: 76 → 3.5, ~22x, close to the ~30x this page
  estimated.
* **The rows are worth ~4.9x on their own** and are largely SUBSUMED by the
  hoist for this shape — once the two loads leave the loop it barely matters
  whether they were helper calls. They are kept because they are not subsumed
  anywhere else: a String access outside a hoistable loop, or in a loop with a
  store in it, still pays the helper without them.
* **The IR tier now TIES the single-pass body** (3.45 against 3.39). The
  "what retirement is gated on" question this page answered as *no, keep the
  pin* is now a tie rather than a 23x loss — still not a reason to retire the
  pin, and now a much cheaper thing to be wrong about.
* Both switches are per-binary: `CRATONVM_JIT_NO_LICM_READ_HOIST=1` and
  `CRATONVM_JIT_NO_STRING_ACCESS_INLINE_ROWS=1`.

### 3. The enumeration — and it is not `{String}`

This page asked for the other final classes with an instance call-site entry
and guessed the set might be `{String}` alone. Walked row by row over the
kind-0 ladder, the set of **final** declared classes is
**`{String, Integer, Long}`** — `VarHandle`, `ByteBuffer`, `MessageDigest`,
`HashMap`, `CRC32`, `CRC32C`, `AtomicInteger` and `AtomicLong` are none of them
final, and `CharSequence` / `ConcurrentMap` / `MemorySegment` are interfaces.

The other two were not covered, because the yield asked the two INTRINSIC
matchers and `Integer.intValue` / `Long.longValue` are served by a **thin
direct-helper bind** instead — a different row of the same ladder, ending in
the same `continue`, which neither matcher knows about.

`probes/FinalDevirtInstanceIntrinsics.java` (added), ns per unboxing, n=12 at
reps=200:

| site | HotSpot | before | after | `CRATONVM_JIT_FINAL_DEVIRT=0` |
|---|---:|---:|---:|---:|
| `Integer.intValue()` | 0.46 | **76.10** | **1.77** | 1.72 |
| `Long.longValue()` | 0.63 | **423.55** | **1.57** | 1.59 |

43x and **270x**. Note the shape is not `String`'s: both alternatives are a
CALL. They are not the same call — the bind the rewrite installs is a Java
frame with a shadow-stack push and an invocation-counter increment, and the
helper the ladder installs is a leaf Rust call that reads the box's field.
That `Long.longValue` costs 424 and `Integer.intValue` 76 for the same
structure is itself unexplained and is the one thing this row leaves behind;
what is closed is that the rewrite no longer decides it.

After the fix `CRATONVM_JIT_FINAL_DEVIRT=0` no longer moves either row (1.77
vs 1.72, 1.57 vs 1.59), which is what "the yield fired" looks like from
outside — the same reading this page's own fix is verified by.

## Still open

* **HotSpot vectorises this loop and we do not.** At ~3.4 ns/char against
  0.47, the inline decode still does, per character, a receiver null check,
  two field reads that are now hoisted, an `arraylength`, a bounds check, two
  byte loads and a branchless combine. That is a general codegen statement —
  the `char[]` control in the same run reads ~1.0 against HotSpot's ~0.3, so
  most of what is left is not about `String` at all. It belongs to the
  vectorisation backlog rather than to this page, and the `charAt`-specific
  part of it (the single-pass region does not hoist `value` and `coder`
  either — the same transform, in the other emitter) is now worth far less
  than it was, because the tier that DOES hoist them ties it.

## A note on the build

The measurements marked "after" were taken on a `lto=false,
codegen-units=16` build. The fat-LTO link was OOM-killed three times on the
shared host (1-3 GB available against a 31 GB box under load 60-96), and the
A/B this binary has to carry is **within one binary**
(`CRATONVM_JIT_NO_DEVIRT_INTRINSIC_YIELD`), where the link profile cancels.
The `before` row is a fat-LTO build and is quoted for shape, not arm-for-arm.
The independent confirmation on a fat-LTO binary is the flag reading:
`CRATONVM_JIT_FINAL_DEVIRT=0` moved `CharAtCostCurve`'s `charAt` rows from
341-361 to 3.17-3.54 on the unmodified dev binary.

## Reproducers

```sh
javac -g -d probes probes/CharAtDoorProbe.java

java     -cp probes CharAtDoorProbe     # the oracle
cratonvm -cp probes CharAtDoorProbe     # all five arms agree
CRATONVM_JIT_NO_DEVIRT_INTRINSIC_YIELD=1 cratonvm -cp probes CharAtDoorProbe
CRATONVM_JIT_FINAL_DEVIRT=0              cratonvm -cp probes CharAtDoorProbe

# engagement — a zero here means the yield never fired
CRATONVM_DBG_JIT_METHOD_STATS=1 cratonvm -cp probes CharAtDoorProbe 2>&1 \
  | grep -E 'devirt yielded|String-intrinsic pin'

# which door, and what it decided
CRATONVM_DBG_JITC=1 cratonvm -cp probes CharAtDoorProbe 2>&1 \
  | grep -E 'full-compile|OSR-compile|devirt YIELDS'
```

`probes/CharAtCostCurve.java` (the witness), `probes/CharAtWarmShape.java`
(the fast shape), `probes/CharAtFirstCompile.java` (hypothesis 4).
