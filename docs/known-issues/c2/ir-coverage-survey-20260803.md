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
