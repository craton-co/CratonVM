# COV-03 — retired 2026-08-03

The lane brief was
`docs/known-issues/c2/cov-03-field-stores-and-wide-fields.md`. It owned the
`0xb4` / `0xb5` arms of `IrBuilder::build` and their lowering in
`jit/src/ir_lower.rs`, and it was sized at **43 events**: 37 on the `putfield`
tag bail and 6 on the `getfield` wide-field bail
(`docs/known-issues/c2/ir-coverage-survey-20260803.md`).

Both are now zero. This file records what the lane found, what landed, what was
measured, and — the part a retired brief usually loses — **what it did not do**.

## The finding the brief asked for

The brief said: *"the asymmetry is probably not an oversight"*, named two
candidate reasons, and asked which one it actually was, with the instruction to
say so plainly if it turned out to be neither.

It is the **write barrier**, and only the write barrier.

The brief's second candidate — the compact-layout split — is not a reason at
all. `jit_putfield_object` resolves the packed offset from the receiver's
registered layout itself (`jit_compact_field_slot`), exactly as
`jit_putfield_int` does for the int case. There was never a compact-layout
question to answer for a reference store; there is one helper and it is correct
under both layouts.

What is real is that a reference **store** needs things a reference **load**
does not:

* the **SATB pre-barrier** on the reference being overwritten, so a concurrent
  marker does not lose an object reachable only through that slot, and
* the collector's own **post-write barrier** — the card / remembered-set edge
  through which an OLD object's field keeps a YOUNG object reachable.

A reference LOAD needs neither. So the change that taught `getfield` about
reference fields had nothing to say about the arm twenty lines below it, and
nothing carried it across. That is the whole mechanism, and it is worth counting
as the brief asked: **"the fix was applied to one of two sites" is a pattern**,
not an accident, and the shape of it here is *"the two sites needed different
things, so the reasoning for one did not travel to the other."*

## What landed

`fix/ir-c2-ref-putfield-wide-20260803`.

**One classifier for both arms.** `IrBuilder::field_access_type` decides the
admitted tag set, and the `getfield` and `putfield` arms both call it. A tag one
arm admits and the other refuses is how the drift arose in the first place, and
it is invisible to any test that exercises one arm;
`test_ir_getfield_and_putfield_admit_the_same_field_tags` compares the two
directly across every gate combination.

**A reference store is `Op::Store(MemKind::Ref)` → always
`jit_putfield_object`.** No inline route and no layout-conditional choice. It is
the identical helper the single-pass backend's reference `putfield` arms fall
back to (`x64::objects::emit_ref_putfield_helper_call`). `lower_inner` REFUSES
the graph when the helper address is absent rather than emitting the store
without its barrier — the brief's own rule, and the right way round, because a
missing barrier is invisible until a concurrent or generational collection and
it surfaces as a lost object rather than as a fault at the store.

**`needs_context`.** `jit_putfield_object` is the one `putfield_*` helper that
takes the VM context pointer, so `scan_frame_needs` marks a reference store
`needs_context`. Without it the lowering would read arg0 from an unreserved
frame slot and hand the helper a stack address to dereference as a `SharedVm` —
the single-pass backend's identical `needs_heap` bug on
`Catalina.setParentClassLoader`, which shipped once already.

**Wide fields, both arms.** `J`/`F`/`D` lower through the checked
`jit_getfield` and the matching `jit_putfield_{long,float,double}`. A wide READ
reuses the out-of-band `jit_dispatch_threw` peek that `Op::Call` already uses
for a `J`/`D` return, because `Long.MIN_VALUE` — and the bits of `-0.0` — are
bit-identical to the helper's `i64::MIN` deopt/NPE sentinel. Without the peek
the lowering would have to either drop a real NPE or bail on a legitimate value;
it refuses to lower instead when the peek is unwired.

**The wide arms are gated on `ir_emit_long` / `ir_emit_fp`, passed down
explicitly.** A wide field is the one way a category-2 or FP value can enter the
graph with no category-2/FP OPCODE in the body — `getfield J; invokestatic (J)V`
contains neither — so `method_uses_category2` / `method_uses_fp`, which are the
admission chain's opcode-and-descriptor scans, would have admitted such a method
with the long/FP tier off. Both flags are **default-ON** in production
(`env_cache::jit_ir_long` / `jit_ir_fp` are `map_or(true, …)`), so the wide arms
are live by default; the builder's own default is both-off, which is the
pre-lane behaviour for a hand-built graph.

**The compact-layout refusal in `lower_inner` is now per-kind.** It checked
`putfield_int` for EVERY `Op::Store`, which would have refused a reference store
for the absence of a helper it never calls.

## Measured

### Coverage — `CRATONVM_DBG=ir-compiles`, `ConditionalOnPropertyTests`

Two rounds, both arms per round, arm order reversed in round 2. **Measured
twice**, because `cov-01` and `cov-02` merged into `dev` while this lane was in
flight and neither of their post-measurements includes this one either. The
second table is the one that describes `dev` today.

**(a) In isolation, against `dev` before `cov-01`/`cov-02`:**

| arm | admitted | bodies | builder refusals |
|---|---:|---:|---:|
| base | 693 / 695 | 410 / 410 | 83 / 83 |
| **fix** | 696 / 694 | **445 / 444** | **50 / 49** |

**(b) On top of `cov-01` + `cov-02`, which is where it landed:**

| arm | admitted | bodies | builder refusals |
|---|---:|---:|---:|
| dev base | 695 / 694 | 537 / 536 | 127 / 127 |
| **merged** | 694 / 696 | **574 / 576** | **89 / 89** |

Refusal sites for (b) (`ir.rs` line numbers shift by +79 with this edit; the
mapping is by arm, and every surviving site maps 1:1):

| site (dev) | what | dev | site (merged) | merged |
|---|---|---:|---|---:|
| `ir.rs:5337` | `putfield` tag not `I/Z/B/C/S` | **38** | `ir.rs:5416` | **0** |
| `ir.rs:5293` | `getfield` of `long`/`float`/`double` | **5** | `ir.rs:5372` | **0** |
| `ir.rs:5456` | `invokespecial` (`cov-04`) | 73 | `ir.rs:5535` | 78 |
| `ir.rs:5516` | invoke with no `invoke_info` (`cov-04`) | 9 | `ir.rs:5595` | 9 |
| `ir.rs:5471` | (`cov-04`) | 1 | `ir.rs:5550` | 1 |
| `ir.rs:5381` | `getfield` with no resolved layout | 1 | `ir.rs:5460` | 1 |

Both of the lane's sites are gone: **43 events removed, 38 net** (`cov-04`'s
`invokespecial` absorbs 5), and bodies rise by **38.5**. Tests: 38/38 pass on
every arm of every run.

The reason 43 removed does not become 43 bodies is the same shape `cov-01` and
`cov-02` both hit: a method that gets past this lane's site dies at the next gap
it meets. That is not a shortfall to explain away — it is the survey's own
prediction, and it is why the README says to re-run the survey after ANY lane
lands.

### Wall clock — and the fake 1.7x regression on the way there

The `ir-compiles` runs above took 35 s / 33 s (base) against 60 s / 57 s (fix),
on **both** orderings. That reads as a consistent 1.7x regression and it is not
one.

Two things were wrong with it, and both are worth carrying forward:

* it was timed with `CRATONVM_DBG=ir-compiles` on, which is a different
  workload — use the debug run for COUNTS (load-insensitive) and a clean run for
  times; and
* "round-robin, reversed in round 2" produces the sequence **A B B A**, which
  puts both `fix` runs adjacent in time. One load spike in that middle window
  lands entirely on one arm, and the reversal that was supposed to protect
  against drift is what created the cluster. Two samples per arm cannot
  attribute a wall-clock difference on this host regardless of ordering.

Re-measured with no debug flags, arms alternating per rep, n=17 per arm
(a 5-rep run and a 12-rep run, host 1-min load 10–17 throughout):

| arm | n | median | mean | range |
|---|---:|---:|---:|---|
| base | 17 | 35.17 s | 36.81 s | 28.85 – 47.17 |
| fix | 17 | 37.50 s | 35.84 s | 26.18 – 47.94 |

**The median and the mean disagree on the sign** (median says `fix` is 6.6%
slower, mean says 2.6% faster) against a per-arm spread of ±6 s. There is no
effect resolvable at this sample size on this host. That is the honest finding:
not "no regression", but "none detectable, and the run-to-run noise is three
times any difference either statistic claims".

### Correctness

* `cargo test -p cratonvm-jit` — **1,875 lib + all integration tests green** on
  Linux. (On a Windows checkout with `core.autocrlf=true`,
  `ir_lower::tests::the_op_representatives_cover_every_declared_variant` fails:
  it `include_str!`s `ir.rs` and splits on `"\n}\n"`, which a CRLF working tree
  does not contain, so the `enum Op` scan overruns into the enums below it. It
  is red on `origin/dev` too and has nothing to do with this lane — but it is
  another instance of `seam-01`'s finding about source-scanning gates, so it is
  named here rather than left for the next person to rediscover.)
* `cargo test -p cratonvm-vm` — **green, 0 failures.**
* **The 78-class Spring Boot regression list** (`/data/data/regr-list.tsv`), run
  against both binaries with the arms interleaved and the order alternating per
  class: **every row identical.** 66 classes pass on both arms, 12 fail
  identically on both (pre-existing — `BootJarTests` 43/43,
  `ModifiedClassPathExtension*`, `WebApplicationTypeIntegrationTests` 5/10, and
  the rest of the gradle-plugin group). No class changed status, test count,
  or failure count in either direction. **Run twice** — once for the isolated
  change and again for the merge on top of `cov-01`/`cov-02` — with the same
  66/12 split both times.
* `jit/tests/ir_vs_singlepass.rs` — the brief's second verification item.
  `ir_vs_singlepass_reference_putfield_then_getfield` stores a reference field,
  reads it back and returns it; `…_pure_write` stores and never reads. Both run
  each backend against its own fresh object and compare the returned reference
  AND the resulting field bytes, for a live object and for null.
* `a_reference_putfield_lowers_only_through_the_barrier_helper` (`ir_lower`) —
  both halves. Refused with no helper wired, and with it wired the emitted
  ARTIFACT bakes the helper's address. "The graph was accepted" and "the barrier
  is in the code" are different claims and a refusal test only makes the first.

### The generational test — and how it was vacuous first

The brief singled this out: *"A reference stored into an old object, pointing at
a young one, surviving a young collection. If the barrier is missing this is the
only test that fails."*

`probes/RefPutfieldBarrierProbe.java` stages it: promote a fleet of holders,
publish a fresh young payload into each through the compiled setter, drop every
other reference to it, churn, read back and check contents.

**The first version passed with `[GC] generational: minor=0 major=0`.** 76 MB of
churn against a 512 MB heap: no collector ever looked at the heap, and the probe
reported PASS. That is precisely rule 5 in the `c2` README — a test that cannot
fail is worse than no test — and the only reason it was caught is that the run
was re-done under `CRATONVM_DBG=gc-stats` to check.

The staged version, at `--Xmx 96m`, with the runner refusing to call a run a
pass unless it can show BOTH a collection count and an optimizing body for the
methods under test:

| backend | cycles | old→young edges recorded | checks | missing | wrong type | corrupt |
|---|---:|---:|---:|---:|---:|---:|
| generational | 46 minor (2 of them MOVING young) | **76,036** | 16,000 | 0 | 0 | 0 |
| G1 | 15 young, all evacuating (`cset_young`, `cset_old=0`) | — | 16,000 | 0 | 0 | 0 |

(Re-run identically on the post-`cov-01`/`cov-02` merged binary: same counts.)

`old_to_young_edges=76036` is the number that makes the run non-vacuous: the
remembered set genuinely recorded the edges, so the barrier path was on the
critical path rather than merely present.

`probes/WideFieldProbe.java` covers the wide half: `Long.MIN_VALUE` and `-0.0d`
(the two values bit-identical to the deopt sentinel), NaN and both infinities by
raw bits, guard fields on either side of the wide slots, and a null receiver on
all six accessors. Green, with all twelve accessors confirmed compiled by the
optimizing backend.

Reproduce:

```bash
CRATONVM_GC=card-metrics CRATONVM_JIT=force-c2 CRATONVM_DBG=gc-stats,ir-compiles \
  cratonvm --Xmx 96m -cp probes RefPutfieldBarrierProbe
```

and check `[GC] generational: minor=N` is non-zero and that
`optimizing backend produced a body` names `set`/`get`. A run without both
proves nothing.

## What this lane did NOT do

Named here rather than left in a summary cell, because that is how the previous
wave lost its residuals (`docs/known-issues/c2/archive/README.md`).

1. **No barrier-free fast path for a reference store.** Every C2 reference
   `putfield` is a helper CALL. The single-pass backend has an opt-in inline
   compact route (`CRATONVM_JIT_INLINE_PUTFIELD`) that writes directly when it
   can prove the receiver is mapped, genuinely compact, YOUNG, and its old field
   null, with live region bounds published. This tier does not, and should not
   get one until it can prove the same four premises — which is a real piece of
   work, not a port.

2. **No inline fast path for a wide field READ either.**
   `emit_inline_compact_getfield` refuses `J`/`F`/`D` tags, so every wide read
   is a helper call plus the `dispatch_threw` cold branch. Six events' worth of
   sites, so this is unlikely to be worth doing on its own.

3. **The brief's stated "first increment" is not what landed.** It proposed
   reference `putfield` on the non-compact path only, refusing when compact
   layout is on. That would have been inert: `compact_ref_fields_enabled()`
   defaults to TRUE, so the refusal would have covered every real receiver, and
   the premise it rested on (that the compact split was the obstacle) is the one
   this lane found to be false. Routing through the helper the baseline tier
   already uses is not a guess — it is the same code path — so the narrower
   increment would have bought nothing but a second landing.

4. **Nothing here is a claim that the optimizing body is FASTER.** See the next
   section; that question is open and it is not this lane's to close.

## The open question this lane surfaced: C2 body quality

See `docs/internal/performance/` and the existing note
`reference_c2_tier_slower_because_fields_take_the_helper`.

Every coverage lane in `docs/known-issues/c2/` increases the number of methods
that take an optimizing body instead of a single-pass one. Whether that is a
win per method is a **timing** question and the survey these lanes are sized
from is explicitly a count.

This lane's own A/B (above) resolves nothing either way at n=17, on one test
class, on a shared host whose run-to-run spread is ±6 s. It is enough to say
"this change did not obviously cost anything"; it is nowhere near enough to say
a C2 body is better than the C1 body it replaced. Do not read it as the latter.

The structural reason to expect trouble is in `ir_lower` and predates this lane:
the IR tier has no general-purpose GPR register allocator on by default
(`ir-linear-scan` is default-OFF), so an integer value lives in a frame slot and
every use is a load. A method that moves from the single-pass backend to the
optimizing one can therefore get slower for reasons that have nothing to do with
which opcodes were newly lowered. That is a tier-quality lane, and it does not
exist yet.
