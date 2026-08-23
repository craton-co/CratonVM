# The Vector API dispatch layer — FIXED 2026-08-23

| | |
|---|---|
| **Status** | **FIXED.** The JDK's route to `VectorSupport` is native for the whole `lanewise` family. What is left is `convert0` and the VM-wide per-call cost, and the page itself already said which of those is a Vector API defect: neither |
| **Opened** | 2026-08-22, as the residual of the lane-at-a-time fallback fixed the same day |
| **Closed by** | `perf/filechannel-vector-webclient-residuals-20260823` |
| **Measured effect** | `Fp16VectorDotBench` **22 093 -> 9 874 ns per lane**, 2.2x, one binary, `CRATONVM_VECTOR_TEMPLATES=0\|1`, three interleaved rounds. `probes/VectorApiProbe.java`: **322 of 322 rows identical** to Temurin 25.0.3+9 in all three arms |

## What the page left open

Its original defect — CratonVM running the JDK's generic, lane-at-a-time Java
fallback for every Vector API operation — was fixed on 2026-08-22 by nine
`VectorSupport` kernels: `fell_back=0`, 3.8x. It stayed open for what was
left, and named it precisely, from its own 342-sample `--nojit` profile:

```text
 48  IntVector.lanewiseTemplate          13  AbstractVector.sameSpecies
 44  AbstractVector.convert0             11  VectorOperators$OperatorImpl.opKind
 19  IntVector.lanewiseShiftTemplate     11  VectorOperators$OperatorImpl.opCode
 18  IntVector$IntSpecies.broadcastBits   9  VectorOperators$ImplCache.find
 11  Int256Vector.lanewise                9  FloatVector.lanewiseTemplate
```

> Going further means intercepting HIGHER — at `Int256Vector.lanewise` and its
> siblings, which are per-shape concrete classes. That is 6 element types x 5
> shapes x the whole operation surface … It is a much larger and more fragile
> surface than the nine static methods below.

## The surface is six classes, not thirty

`Int256Vector.lanewise(op, v)` is one line: `return (Int256Vector)
super.lanewiseTemplate(op, v);`. The template is declared on the six
per-ELEMENT-TYPE classes and every shape funnels through it, so intercepting
the TEMPLATE covers all thirty concrete classes at a sixth of the surface. Five
entry points per class:

| entry point | what it collapses |
|---|---|
| `lanewiseTemplate(Binary, Vector)` | the special-case cascade, `opCode`, `BIN_IMPL.find`, `VectorSupport.binaryOp`, `maybeRebox` |
| `lanewiseTemplate(Unary)` | the same, for `unaryOp` |
| `lanewiseTemplate(Ternary, Vector, Vector)` | the same, for `ternaryOp` (FMA) |
| `lanewiseShiftTemplate(Binary, int)` | the lane-width mask, `opCode`, `BIN_INT_IMPL.find`, `broadcastInt` |
| `lanewise(Binary, <lane scalar>)` | **the broadcast**: `XxxSpecies.broadcastBits`, `fromBitsCoerced`, a whole throwaway vector, plus everything in row 1 |

The last one was not in the page's list and is the largest by call count. The
`Fp16VectorDotBench` census before it existed read `fromBitsCoerced
handled=62525` against `binaryOp handled=93696`: two thirds of the binary
operations arrive as `v.and(0x7C00)` / `v.add(0x1C000)` rather than as
`v.op(otherVector)`, and each of those materialised a whole broadcast vector
and discarded it one operation later.

## An operator is one int, and the decode is the JDK's own arithmetic

`VectorOperators$OperatorImpl` carries a single field, `opInfo`, and the JDK's
accessors are pure functions of it (`javap -c`, Temurin 25.0.3+9):

```text
opCodeRaw()   = opInfo >> 12
opKind(mask)  = (opInfo & mask) != 0
opCode(req,forbid): opCodeRaw(), throwing unless (opInfo & req) == req
                    and (forbid == 0 || (opInfo & forbid) != forbid)
```

Every `XxxVector.opCode` passes `req = 2048`; `forbid` is `256` on the two
floating-point classes and `512` on the four integral ones. Those are the
literals in the templates' own bytecode rather than the names of source
constants, because the bytecode is what runs. Reading `opInfo` and applying
those three lines is therefore not a re-derived table that can drift from the
JDK's — it is the JDK's arithmetic on the JDK's field.

## What refuses

To its own body, through `invoke_special_bytecode_only` — the "run this
bytecode, no native check" primitive — so a refusal is bit-for-bit the
un-intercepted VM:

* every special-case branch not modelled: `AND_NOT`, `DIV`-by-zero,
  `FIRST_NONZERO`, `ZOMO`, `NOT`, `BITWISE_BLEND`. All six carry `VO_SPECIAL`,
  so one mask test refuses all of them;
* a `check()` species mismatch, tested as concrete-class identity — stronger
  than species identity, and it needs no species object;
* any opcode the lane kernels do not compute, and any operator whose `opCode`
  would have thrown;
* every masked form: those are different descriptors and are not registered.

**A `VO_SPECIAL` test where the JDK does not make one costs the whole win.**
The first cut applied it to the unary template as well, and the census said so
immediately: `tmpl:lanewise(Unary) handled=0 fell_back=30976` while the kernel
below it answered all 30 976. `lanewiseTemplate(Unary)` branches on `ZOMO` and
`NOT` by IDENTITY, and both are expansions with no `VectorSupport` opcode, so
`VO_OPCODE_VALID` already refuses them. The binary template keeps its mask
because `AND_NOT` there IS opcode-valid — it rewrites to `AND` of the
complement — so dropping it would be a wrong ANSWER rather than a missed
optimisation. Two adjacent templates, opposite correct answers, and only the
per-entry census separates them.

## The numbers

Windows 11, one binary, `CRATONVM_VECTOR_TEMPLATES=0|1`, three interleaved
rounds of `probes/Fp16VectorDotBench.java 20 2048`:

| | round 1 | round 2 | round 3 |
|---|---:|---:|---:|
| templates **off** (kernels only) | 30 363 | 21 945 | 22 093 |
| templates **on** | **9 405** | **9 874** | **12 280** |

`checksum=1098616832` and `warm=1061765120` on every row of both arms, and
`warm` matches HotSpot's. HotSpot itself is 0.265 ns per lane.

The engagement census is what makes that readable, and it says more than the
wall clock does — templates ON:

```
tmpl:lanewise(scalar)  handled=109312 fell_back=0
tmpl:lanewise(Binary)  handled=31232  fell_back=0
tmpl:lanewise(Unary)   handled=15616  fell_back=0
tmpl:lanewise(Ternary) handled=15616  fell_back=0
convert                handled=46848  fell_back=0
load                   handled=31232  fell_back=0
fromBitsCoerced        handled=61     fell_back=0
```

against templates OFF:

```
binaryOp        handled=93696  broadcastInt handled=46848
unaryOp         handled=15616  fromBitsCoerced handled=62525
ternaryOp       handled=15616  convert handled=46848   load handled=31232
```

`fromBitsCoerced` **62 525 -> 61** and `broadcastInt` **46 848 -> 0** are the
scalar interception removing the broadcasts outright; the total operation count
falls 312 442 -> 249 978 for identical work.

## Correctness

`probes/VectorApiProbe.java` — 322 rows of raw lane bits across six element
types, four species, every implemented opcode, both shift forms, casts,
reinterprets, reductions, loads, stores, and the masked forms the Rust side
deliberately refuses — **differs from Temurin 25.0.3+9 in zero rows**, in all
three arms: templates on, templates off, and `CRATONVM_VECTOR_INTRINSICS=0`.

Regression suite on the same binary: 69 passed, 0 failed.

## What is left, and why it is not this page

Two things, and the page's own text already classified them.

**`AbstractVector.convert0`** (44 of the 342 samples) is the one named item not
taken. Unlike `lanewiseTemplate`, whose whole input is one `int` field, it
needs `AbstractSpecies.asIntegral()`, `dummyVector()`, `laneCount()`,
`elementSize()` and `elementType()` — it is a function of species OBJECTS, not
of a bit field, and modelling `AbstractSpecies` is a genuinely larger surface
than this page's own next step was. The `convert` KERNEL below it is already
native and answers 46 848 of 46 848 with `fell_back=0`; what stays interpreted
is the route.

**Everything else is the VM-wide per-call cost**, which is what the open page
concluded in its own words:

> At CratonVM's measured per-call cost of roughly 200 ns, several dozen calls
> per operation is several microseconds per operation before any lane
> arithmetic happens. **This is the general interpreter/JIT call-cost story,
> not a Vector API defect.**

That story has its own pages —
[[jit-entries-per-call-cost-is-the-call-dense-wall]] and
`known-issues/perf/jit-compiled-caller-to-interpreted-callee-costs-1900ns-20260822.md`
— and it is where the remaining distance to HotSpot lives. **Do not reopen this
page for it.** The distinguishing test is the census: if the `tmpl:` rows carry
the traffic with `fell_back=0`, the dispatch layer is not what is being
measured.

The arithmetic the parent page insisted on stating still holds and is still
worth stating: at ~10 µs per lane a Llama-3.2-1B forward pass of ~1.2e9 lane
multiply-adds is on the order of **3 hours per token**, against ~7 hours before
this change and ~40 hours before the kernels. A speedup that does not cross a
usefulness threshold is still a speedup, and reporting one without the
threshold would be the more misleading of the two.

## Repro

```bash
# correctness: 322 rows, must match a real JDK exactly, in every arm
probes/VectorApiProbe.java
# throughput, the template layer against the kernels it sits on
probes/Fp16VectorDotBench.java      # CratonVM: 20 2048   HotSpot: 2000 2048
```

Run with `--add-modules jdk.incubator.vector --enable-native-access=ALL-UNNAMED`.

```bash
CRATONVM_VECTOR_TEMPLATES=0        # kernels only, route interpreted
CRATONVM_VECTOR_INTRINSICS=0       # neither layer: the un-intercepted VM
CRATONVM_VECTOR_INTRINSICS_STATS=1 # the per-entry census at exit
```

Both switches gate REGISTRATION, so each "off" arm is a VM that never answers
that layer natively rather than one that answers and discards. Read the census
before believing any number: "the templates ran" and "every call fell back
while the host happened to be quieter" produce the same wall clock.
