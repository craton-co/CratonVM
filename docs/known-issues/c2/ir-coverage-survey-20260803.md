# The optimizing tier's coverage, measured 2026-08-03

Every number the `cov-*` lanes are sized from. One run each, ten workloads,
**default configuration** — no `CRATONVM_JIT_FORCE_C2`, no flags beyond the
Spring Boot runner's usual `CRATONVM_JIT=rootsnap-cache`.

Reproduce with `CRATONVM_DBG=ir-compiles`, which prints one `[ir] admission`
line per compile request with the verdict, and one `[ir] optimizing backend
produced a body` line per success. Both are ungated by `metrics::enabled()`.

## Headline

**The optimizing tier runs, and 41% of what it admits it cannot lower.**

| | |
|---|---:|
| compile requests reaching the admission chain | 1,954 |
| …refused `optimize=false` (a C1 request — normal tiering, not a gap) | 719 |
| …refused by `ir_compatible` before the builder saw them | 248 |
| **admitted to the optimizing pipeline** | **982** |
| **bodies the optimizing backend actually produced** | **592** |
| admitted but never lowered | **390** |
| distinct methods admitted / with a body | 848 / 497 |

`optimize=false` is **not** a defect and no lane targets it:
`tier_uses_optimized_backend` is `matches!(tier, C2 | FullProfile)`, so a
`false` here is a C1 request doing exactly what tiering says. C2 was asked for
982 times out of 1,954 — the request rate is healthy. The loss is downstream.

## Per workload

| workload | requests | admitted | `optimize=false` | `ir_compatible` | bodies |
|---|---:|---:|---:|---:|---:|
| `ConditionalOnPropertyTests` | 1,372 | 694 | 537 | 140 | 409 |
| `AutoConfigurationSorterTests` | 352 | 182 | 98 | 72 | 123 |
| `ConditionalOnClassTests` | 223 | 103 | 84 | 36 | 58 |
| **CratonBench, all seven phases** | **7** | **3** | **3** | **1** | **2** |

That last row is `meas-02`. The CPU benchmark suite — the thing the perf gate
measures and the README table publishes — issues **seven** compile requests to
the optimizing tier across all seven phases and gets **two** bodies. Every
conclusion this project has drawn about C2 from a CratonBench number was drawn
from a workload that does not reach it.

## Where the 390 die: opcodes `IrBuilder::build` has no arm for

273 events. `[ir] IrBuilder::build has no lowering for opcode 0xNN`.

| opcode | mnemonic | events | lane |
|---|---|---:|---|
| `0xb2` | `getstatic` | 92 | `cov-01` |
| `0x12` | `ldc` | 90 | `cov-01` |
| `0xbe` | `arraylength` | 43 | `cov-02` |
| `0x32` | `aaload` | 18 | `cov-02` |
| `0x13` | `ldc_w` | 7 | `cov-01` |
| `0x5a` | `dup_x1` | 6 | `cov-02` |
| `0x33` | `baload` | 6 | `cov-02` |
| `0x34` | `caload` | 5 | `cov-02` |
| `0x2e` | `iaload` | 3 | `cov-02` |
| `0x54` | `bastore` | 2 | `cov-02` |
| `0xbc` | `newarray` | 1 | `cov-06` |

`getstatic` + `ldc`/`ldc_w` is **189 of 273 — 69%** of every opcode gap.

### The asymmetry worth staring at

`IrBuilder::build` **does** have arms for `0x30 faload`, `0x31 daload`,
`0x51 fastore`, `0x52 dastore`. It has arms for **no** integral or reference
array access: not `iaload`, not `baload`, not `caload`, not `aaload`, not
`bastore`.

A float array element can be lowered and an `int[]` element cannot. That is not
a design decision anybody would defend out loud; it is the shape of the
fixtures the arms were written against. It is also precisely the failure mode
this directory's own closeout rule names — *"Do not size a lane from a
fixture's node mix"* — showing up in the code rather than in a plan.

## Where the rest die: structural refusals inside the builder

112 events. `[ir] IrBuilder::build refused at ir.rs:NNNN (bytecode pc N)`.

| site | what it is | events | lane |
|---|---|---:|---|
| `ir.rs:5204` | `invokespecial` that is neither a resolvable call nor a trivial `<init>` to elide | 53 | `cov-04` |
| `ir.rs:5085` | `putfield` whose type tag is not `I/Z/B/C/S` — **every reference field store** | 37 | `cov-03` |
| `ir.rs:5264` | an invoke with no `invoke_info` at that pc | 13 | `cov-04` |
| `ir.rs:5041` | `getfield` of a `long`/`float`/`double` | 6 | `cov-03` |
| `ir.rs:5314` | — | 2 | `cov-04` |
| `ir.rs:5219` | — | 1 | `cov-04` |

`ir.rs:5085` is the second asymmetry. The `getfield` arm was taught to handle
reference fields, and its own comment records why: *"This was the single largest
builder refusal measured on a real workload (66 of 141 on a Hibernate class): a
reference field read is most of what object-oriented Java does."* The `putfield`
arm twenty lines below it was not given the same treatment, and a reference
field **write** is most of what object-oriented Java does too.

## The whole-method refusals, before the builder runs

583 events, from `ir_compatible` (`jit/src/ir.rs:6066`). These methods never
enter the builder at all, so they are absent from the 273 and the 112 above.

| conjunct | events | lane |
|---|---:|---|
| `!scan.typecheck_ops.is_empty()` (`checkcast` / `instanceof`) | 306 | `cov-05` |
| `!scan.anewarray_ops.is_empty()` (`0xbd`) | 138 | `cov-06` |
| `scan.has_athrow` | 89 | `cov-07` |
| `!scan.indy_ops.is_empty()` | 44 | — |
| `scan.field_ops.len() > IR_MAX_FIELD_OPS` | 4 | — |
| `!scan.multianewarray_ops.is_empty()` | 2 | `cov-06` |

`indy` has no lane on purpose: `invokedynamic`'s single-pass lowering is an
unconditional trap that publishes a resume snapshot, and giving the IR tier a
second lowering for it is a much larger question than the 44 events justify.
Write it up before starting it, not after.

## Addendum — the `CRATONVM_JIT_FORCE_C2=1` arm

> Measured **before** `cov-01` landed. Its "default" column is therefore the
> pre-`cov-01` tier, and the `0xb2` / `0x12` / `0x13` rows in its opcode table
> are now zero in both arms — see the next section. Its *conclusions* are
> unaffected and one of them was the reason to run `cov-01` first.

Run the same day, same binary, same ten workloads, to answer the question the
`cov-*` lanes rest on: **is a C2 body better than the C1 body it replaces?** If
it is not, widening IR coverage makes things worse faster, and this project has
a recorded case where the optimizing tier was 1.85x slower because fields took
`jit_getfield`.

`CRATONVM_JIT_FORCE_C2=1` treats every compile request as a C2 request, so it
removes the 719 `optimize=false` refusals and routes the whole population
through the optimizing pipeline.

| | default | force-c2 |
|---|---:|---:|
| compile requests | 1,947 | 1,949 |
| `optimize=false` | 719 | **0** |
| refused by `ir_compatible` | 243 | 446 |
| admitted | 984 | **1,502** |
| bodies produced | 595 | **886** |
| **distinct methods with a body** | **495** | **495** |

### Correctness: clean

61/61 Spring Boot tests pass in both arms. All seven CratonBench checksums
match HotSpot in both arms. Zero panics, zero SIGSEGVs, zero internal errors,
zero compile bails, and the same 16 warnings in each — i.e. forcing 886 IR
bodies through this workload introduces nothing.

That is worth stating plainly because it is the first time this tier has been
run at that volume on framework code: the pipeline is *correct* at 886 bodies.

### The finding: FORCE_C2 is not a coverage lever

886 bodies against 595, over **the same 495 distinct methods** — `comm` on the
two sorted method sets reports **zero** on both sides. Not a coincidence of
counts; the sets are identical.

So the extra 291 bodies are repeat compiles of methods the default arm already
lowered. Forcing C2 changes *when* the optimizing tier is used, never *which*
methods it can serve. A method C2 cannot lower, it cannot lower no matter how
early it is asked.

The consequence for this directory is direct: **the `cov-*` lanes are the only
lever on coverage.** There is no configuration that buys what they buy.

### The demand curve is stable, which de-risks the lane sizing

The obvious objection to the main survey is that it measured only the methods
tiering chose to promote — a biased sample. The FORCE_C2 arm is the whole
population, and it says the bias does not matter:

| opcode | mnemonic | default | force-c2 |
|---|---|---:|---:|
| `0xb2` | `getstatic` | 91 | 148 |
| `0x12` | `ldc` | 90 | 132 |
| `0xbe` | `arraylength` | 44 | 68 |
| `0x32` | `aaload` | 18 | 29 |
| `0x13` | `ldc_w` | 7 | 13 |
| `0x33` | `baload` | 6 | 12 |
| `0x5a` | `dup_x1` | 6 | 9 |
| `0x34` | `caload` | 5 | 8 |
| `0x2e` | `iaload` | 3 | 5 |
| `0xbc` | `newarray` | 1 | 2 |
| `0x54` | `bastore` | 1 | 1 |

**No opcode appears in the force-c2 arm that is absent from the default arm.**
The curve scales by roughly 1.5x and the ranking is unchanged;
`getstatic` + `ldc` + `ldc_w` is 293 of 429 events (68%) against 69% before. The
`cov-01`/`cov-02` ordering holds for the full population, not just the promoted
subset.

That prediction was then tested rather than left standing: `cov-01` landed the
same day and took its three rows to zero on the default arm, which moved
`cov-02` to the whole of the remaining opcode gap — exactly the ordering this
table said to expect. Nobody has re-run the force-c2 arm since; when someone
does, the `0xb2` / `0x12` / `0x13` rows should be zero there too, and that is a
cheap check on whether this arm still measures what it says.

The whole-method conjuncts do **not** scale uniformly, and two of them move up:

| conjunct | default | force-c2 | ratio |
|---|---:|---:|---:|
| `typecheck_ops` | 304 | 506 | 1.7x |
| `anewarray_ops` | 134 | 184 | 1.4x |
| `has_athrow` | 85 | **167** | **2.0x** |
| `indy_ops` | 44 | **116** | **2.6x** |
| `IR_MAX_FIELD_OPS` | 4 | 4 | 1.0x |

`athrow` and `invokedynamic` are relatively more common in the methods tiering
does *not* promote. That does not change `cov-07`'s framing — it is still a
question, and "keep the refusal" is still a legitimate answer — but 167 is the
number to argue against, not 85. The same goes for the indy question this
directory currently declines to open at 44: the full-population figure is 116.

### Speed: still not measured, and not for want of trying

The single-run wall clocks favoured force-c2 on all three classes, and a
4x-interleaved re-run of `ConditionalOnPropertyTests` favoured it in 3 of 3
completed pairs. **None of that is evidence.** The host's 1-minute load went
from 9 to 61 during the interleave — the *same* arm measured 67 s at load 24
and 166 s at load 61 — and because the arms ran in a fixed order within each
pair, the decaying spike biased every pair the same way. That is precisely the
confound `feedback_interleave_ab_arms_never_run_them_in_separate_blocks` was
written for, arriving through ordering rather than through blocking.

What can be said: **nothing showed the feared regression.** The 1.85x-slower
shape did not reproduce, at 886 IR bodies, on framework code. That is enough to
say the `cov-*` programme is not self-defeating; it is not enough to say it
pays. Settling it needs `run-cratonbench-gate.sh` on a quiet host — and per
`meas-02`, a workload that actually reaches the tier, which CratonBench does
not.

## Re-measured after `cov-01` landed, same day

`cov-01` closed (`docs/internal/cov-01-constants-and-statics-RETIRED-20260803.md`).
Its three opcodes are at **zero**. Everything below is the same command on the
same workload — `ConditionalOnPropertyTests`, default configuration,
`CRATONVM_DBG=ir-compiles` — with the arms **interleaved, two runs each and in
both orders**. Interleaving matters even for counts: tiering is time-driven, so
the number of compile requests a run issues drifts by a few, and a
block-per-arm layout would attribute that drift to the change.

| | base | after `cov-01` |
|---|---:|---:|
| admitted to the optimizing pipeline | 694–695 | 694–695 |
| **bodies the optimizing backend produced** | **410** | **501–502** |
| `IrBuilder::build returned None` | 281–282 | **180** |

**+91 bodies, +22%**, from 155 opcode-gap events removed. The two numbers do
not match, and the gap is the thing to read: a method that stops failing on
`0x12` immediately fails on whatever it meets next. Most of them met `cov-04`.

All three Spring workloads, same protocol, `cov-01` opcode-gap events
(`0x12` + `0x13` + `0xb2`) in the last column:

| workload | bodies before | bodies after | Δ | gaps before → after | tests |
|---|---:|---:|---:|---|---|
| `ConditionalOnPropertyTests` | 410 | 501–502 | **+91 (+22%)** | 155 → **0** | 38/38 |
| `AutoConfigurationSorterTests` | 123–124 | 142 | **+18 (+15%)** | 23 → **0** | 18/18 |
| `ConditionalOnClassTests` | 57–58 | 63 | **+6 (+10%)** | 12–13 → **0** | 5/5 |
| **total** | **590–592** | **706–707** | **+116 (+20%)** | 190–191 → **0** | — |

The 590–592 figure is the same 592 the headline table reports for all ten
workloads, because the bench phases contribute two bodies and the three Spring
classes contribute the rest. Every arm passes its tests and none crashed.

| opcode | mnemonic | before | after | lane |
|---|---|---:|---:|---|
| `0xb2` | `getstatic` | 72 | **0** | ~~`cov-01`~~ |
| `0x12` | `ldc` | 77 | **0** | ~~`cov-01`~~ |
| `0x13` | `ldc_w` | 6 | **0** | ~~`cov-01`~~ |
| `0xbe` | `arraylength` | 28 | 31 | `cov-02` |
| `0x32` | `aaload` | 10 | 12 | `cov-02` |
| `0x2e` | `iaload` | 3 | 6 | `cov-02` |
| `0xbc` | `newarray` | 1 | 3 | `cov-06` |
| `0x5a` | `dup_x1` | 1 | 1 | `cov-02` |
| `0x33` | `baload` | 1 | 1 | `cov-02` |
| `0xb3` | `putstatic` | 0 | **1** | nobody |

**The whole remaining opcode gap is `cov-02`'s** — 51 of 55 events. `putstatic`
appears for the first time: it was always there, hidden behind the `getstatic`
in the same method, and `cov-01` deliberately does not own it (a static
reference WRITE owes an SATB pre-barrier that no collector `set_field` barrier
covers, which is why the single-pass backend keeps writes on their helpers).

The structural refusals are where the shortfall went. Line numbers are
post-change:

| site | what it is | before | after | lane |
|---|---|---:|---:|---|
| `ir.rs:5331` | `invokespecial` that is neither resolvable nor a trivial `<init>` | 36 | **71** | `cov-04` |
| `ir.rs:5212` | `putfield` whose type tag is not `I/Z/B/C/S` | 33 | 38 | `cov-03` |
| `ir.rs:5391` | an invoke with no `invoke_info` at that pc | 8 | 9 | `cov-04` |
| `ir.rs:5168` | `getfield` of a `long`/`float`/`double` | 5 | 5 | `cov-03` |
| `ir.rs:5346` | `invokespecial <init>` whose receiver is not a fresh `new` | 1 | 1 | `cov-04` |
| `ir.rs:5256` | a `new` with no entry in `new_info` | 0 | **1** | nobody |

`cov-04`'s largest refusal **doubled without anyone touching it** — 36 to 71.
That is this directory's own re-run rule playing out one level down from where
it was written: lifting a refusal admits the methods that were hiding behind
it, and they fail on whatever they meet next. `cov-04` is now the single
largest thing between the optimizing tier and the 180 methods it still
declines, and it is the lane the README already says cannot be sized from a
survey.

`ir_compatible`'s whole-method conjuncts moved by 1–2 events each, i.e. not at
all: they are decided before the builder runs, so `cov-01` could not have moved
them and did not.

One knock-on outside the tier, recorded because it is the shape to expect from
every remaining `cov-*` lane: a method moving from C1 to C2 takes it **out of
reach of every single-pass-only capability**. `vm/tests/pgo02_guarded_virtual_inline.rs`
was passing because the IR builder refused `getstatic`, so its
`getstatic; invokevirtual` fixture stayed on the single-pass backend where
guarded monomorphic inlining is planned. It now pins that tier explicitly.

## What this survey does NOT say

* Nothing here is a **timing**. Every number is a count, so the loaded host
  (1-min load 10–14 throughout) does not affect it. No conclusion about speed
  can be drawn from this file.
* It does not say lowering these opcodes makes anything **faster**. It says the
  optimizing tier declines to compile 41% of what it admits. Whether an
  optimized body beats the single-pass one for a given method is a separate
  measurement, and `docs/internal/performance/` has at least one case where it
  did not (`reference_c2_tier_slower_because_fields_take_the_helper`).
* Nine of the ten workloads are Spring-shaped. The bench phases are the only
  counter-sample and they are the row that reaches nothing, so the opcode
  ranking above is a **Spring** ranking. A lane that wants to claim a general
  ranking should add a second family — H2 or Tomcat — before quoting these
  numbers as the ordering.
