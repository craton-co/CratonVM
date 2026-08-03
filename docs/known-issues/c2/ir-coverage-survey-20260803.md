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

> **Stale since `cov-04` landed (2026-08-03).** Re-measured on the same three
> workloads with the invoke terms removed, this table reads 285 events, not 273:
> `ldc` 90→**100**, `getstatic` 91→**94**, `ldc_w` 7→**8**, `aaload` 18→**19**,
> `dup_x1` 6→**7**. The rest are unchanged. `cov-01` is still 69% and still the
> largest bucket, but re-derive before sizing anything from these rows.

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
| `ir.rs:5204` | `invokespecial` that is neither a resolvable call nor a trivial `<init>` to elide | 53 | ~~`cov-04`~~ CLOSED |
| `ir.rs:5085` | `putfield` whose type tag is not `I/Z/B/C/S` — **every reference field store** | 37 | `cov-03` |
| `ir.rs:5264` | an invoke with no `invoke_info` at that pc | 13 | ~~`cov-04`~~ CLOSED |
| `ir.rs:5041` | `getfield` of a `long`/`float`/`double` | 6 | `cov-03` |
| `ir.rs:5314` | — | 2 | ~~`cov-04`~~ CLOSED |
| `ir.rs:5219` | — | 1 | ~~`cov-04`~~ CLOSED |

**The four `cov-04` rows are one cause, not four, and this table says so
misleadingly.** Closed 2026-08-03 —
[`docs/internal/cov-04-the-invoke-arms-RETIRED-20260803.md`](../../internal/cov-04-the-invoke-arms-RETIRED-20260803.md).
All 69 events are an `<init>`: two whole-method terms discarded the method's
entire `invoke_info` map, and the builder then bailed at whichever invoke came
first in bytecode order. That is why `5264`'s callees are `StringBuilder.append`
and `Class.getName` — sites that were never the problem. A bail site records
where a method died; it does not record why, and a row that reads like an
opcode-shaped gap can be neither.

> **Also stale since `cov-04` landed.** With the four `cov-04` rows at zero the
> table reads 66 events, not 112, and `ir.rs:5085` is **59**, not 37 — it is now
> the largest builder refusal in the corpus by a wide margin. The methods that
> were hiding behind the invoke terms are constructors, and constructors write
> reference fields, which is `cov-03`'s row.

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
