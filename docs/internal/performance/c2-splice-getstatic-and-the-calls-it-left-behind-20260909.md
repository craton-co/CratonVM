# The optimizing tier's spliced calls were name resolutions, and a `getstatic` in a callee cost it the whole inline

Slug: `c2-splice-getstatic-and-the-calls-it-left-behind` · 2026-09-09
Follows `d21ae9e3f` ("the C2 tier's cost was compile time paid 502 times, not
slow code"), and revises one of its conclusions.

---

## VERDICT

That commit's finding was that the optimizing tier's visible cost was
recompilation, and that "the C2 *body* is throughput-neutral against the
single-pass body once compile cost is amortized". The first half stands. The
second half is true of the shapes it was measured on and **false of the shape
framework code is mostly made of**: a hot method whose callees read statics, or
whose spliced body still makes a call.

Measured per BODY, not per run (see §1 on why a run median is meaningless
here):

| body | before | after | |
|---|---|---|---|
| single-pass (`C2_ACCEPT=never`) | 56 ms | 56 ms | unchanged, as it must be |
| **optimizing, statics behind accessors** | **~830 ms** | **~23 ms** | 15x WORSE than C1 → 2.4x better |
| **optimizing, a call surviving a splice** | **~455 ms** | **~57 ms** | 8.5x worse than C1 → parity |

Two causes, both PLUMBING and neither modelling:

| | cause | effect |
|---|---|---|
| 1 | a callee containing `getstatic` was refused for splicing, because nothing rebased its (already resolved) rows | the accessor stayed a call |
| 2 | a statically-bound call that SURVIVED a splice got no `ir_direct_calls` row, and the main compile path passed the resolver no binder to produce one | that call became a **name resolution**, not a call |

The second is worse than not splicing at all: the resolver had bound the callee
entry and registered it on the artifact's KEEP-ALIVE list, so the compile paid
to pin a target for a direct call it then never emitted.

Neither was protected by anything on purpose. The default acceptance policy
(`evidence`) abandoned the supersede on most runs for an unrelated reason —
`is_worth_publishing` saw no admissible transform — and a run where it did not
got the slower body. That is a defect wearing the costume of a policy.

---

## 1. The measurement that found it

`bench/SpliceStaticProbe.java` — a hot method whose callees are
`return someStatic;`. Nothing exotic: `static final int` is folded to an `ldc`
by javac, so the probe's statics are non-final, which is what a registry, a
cache or a configured limit is. `bench/SpliceCallProbe.java` is the same shape
for cause 2: a spliceable `mid` that calls a `tableswitch`-bearing `pick` no
resolver will splice, so the call lands at a combined-buffer pc.

**A run median is meaningless on this workload, and saying so is part of the
result.** Whether the optimizing body is installed before the timed loop starts
is a race with the background compiler that a longer warm-up does not settle:
the same binary, same arm, produced 57 ms and 863 ms on alternate runs. Those
are not a distribution, they are two bodies. So the arms are separated by which
body ran, which `CRATONVM_C2_ACCEPT=never` pins from the other side.

Windows 11 / this dev box, one binary, `SpliceStaticProbe`, every sample
checksum-verified against Temurin JDK 25 (`-174644736` throughout):

```
never (single-pass body only):  54 54 55 56 58 55 57 54
always, both switches OFF:      58 53 56 57 59 57 808 53 54 853
always, both switches ON:       55 59 23 23 58 63 23 54 60 55
```

`never` has one mode and it is the single-pass body at ~56 ms. Each `always`
arm is that same mode plus a second one — the optimizing body — at ~830 ms
before and ~23 ms after.

`SpliceCallProbe`, same method, checksum `863040228` throughout:

```
never:                     53 52 53 56 54 52
always, DIRECT_CALL=0:     52 55 52 468 53 448 53 53
always, DIRECT_CALL=1:     57 55 57 62 58 58 57 54
```

The second mode is at ~455 ms before and gone after — the optimizing body is at
parity with the single-pass one instead of 8.5x behind it. The census row from
§4 reads `in_splice=2` on the 439 ms run and `in_splice=0` with the fix on,
which is the same fact without a stopwatch.

## 2. Why `getstatic` cost the callee its inline

`resolve_inline_site_from` refused any callee containing `getstatic` for the
optimizing tier, under the name `ir-splice-static-field`, with this reason:

> No `static_field_info` is rebased into the builder's tables, so a spliced
> `getstatic` would find no row and bail the whole method AFTER the walk had
> committed to the body.

An accurate description of the plumbing, and not of any modelling problem.
`InlineSite::static_field_info` has carried resolved
`(pc, class_id, field_index, type_tag, is_volatile)` rows for the single-pass
inliner since it existed. `IrInlineTables` now rebases them into
combined-buffer coordinates and `IrBuilder`'s own `0xb2` arm reads a spliced
site exactly as it reads one of the caller's.

Three things ride along, and each is a place the shorter version would have
been wrong:

* **The value tier.** `getstatic` is polymorphic and is listed by neither
  `is_category2_opcode` nor `is_float_opcode`, so a callee whose only wide
  content is a static read reaches the splice inside a method admitted through
  the INT clause. `append_ir_inline_site` applies the same `ir_emit_long` /
  `ir_emit_fp` gate `try_compile_inner` applies to the caller's own feed — and
  REFUSES the body rather than omitting the row, because omitting it there
  bails to single-pass while omitting it here bails a method already committed
  to the splice.
* **The ensure-init obligation.** Compiled code reads static storage directly,
  so each spliced site's declaring class joins `static_init_classes`, which the
  compiled-entry path runs `<clinit>` for once per artifact. Collected
  per-site and merged only on success, so a rolled-back body leaves no debt.
* **`putstatic` stays refused**, and that one IS modelling: the builder has no
  `0xb3` arm at all, and a static reference write owes an SATB pre-barrier that
  lives on the single-pass `jit_putstatic_*` path — statics are a Rust-side
  table, not the heap, so no collector `set_field` barrier covers them. The
  rebase re-reads the opcode from the relocated bytes and refuses a row that is
  not `0xb2`, so a resolver defect cannot plant a load lowering at a store.

`CRATONVM_JIT_IR_SPLICE_GETSTATIC=0` restores the refusal.

## 3. The calls the splice left behind were name resolutions

This is the one that mattered, and it was invisible because two halves of it
each looked finished.

`resolve_inline_site_from` binds `direct_entry` for every `invokestatic` /
`invokespecial` in a body it is about to splice, and
`intern_inline_invoke_targets` registers that entry in
`direct_callee_entries` — the KEEP-ALIVE list `prepare_for_publication` pins
the callee artifact through, so a baked address cannot dangle. Everything a
direct call needs was resolved, and the artifact was already paying to hold the
target alive.

Nothing put the row where the lowerer looks. `ir_direct_calls` was filled only
from the CALLER's own scan loop over its own invoke sites, so
`self.direct_calls.get(&pc)` missed at every combined-buffer pc, and the call
fell through to `jit_invoke_dispatch` — which resolves the callee **by name on
every execution**.

Two comments in the tree said the opposite, and both were written as statements
of intent:

> `resolve_inline_site_from`: "`IrBuilder` lowers a spliced call to the same
> `Op::Call` it lowers every other invoke to, and `ir_lower` gives it the
> ordinary dispatch. … it is a call, not a resolution."

> `jit_bridge`: "No direct-bind resolver: a spliced body's remaining calls go
> through the dispatch helper on this path, and `IrBuilder` has no direct-call
> lowering inside a relocated body to bake an entry into."

The second is the load-bearing one: the main compile path passed
`resolve_ir_inline_site` **no binder at all**, so `direct_entry` was `None` for
every spliced call and the rows would have been empty even if something had
asked for them. Both halves are now supplied — the binder is the same
lookup-only `direct_callee_lookup` the single-pass sibling uses, which binds an
already-compiled callee and never compiles one, so planning still cannot
recurse into compilation.

The single-pass side reached the same conclusion in 2026-08 and enforced it
with a refusal ("a spliced call must not be WORSE than the call it replaced"),
measuring 47 ns/iter against 163-266 when an admitted call went blind. The
optimizing tier was exempted from that refusal on the strength of the comment
above.

`CRATONVM_JIT_IR_SPLICE_DIRECT_CALL=0` restores the dispatch helper, and both
halves read the same switch so neither can be flipped alone.

## 4. It now has a reading

`[c2-supersede] ir blind dispatches: own_code=N in_splice=M` under
`CRATONVM_DBG_JITC`.

The split is the point. A blind dispatch in a method's own code is ordinary —
an unbindable kind, a site with no inline cache. One **inside a spliced body**
is a contradiction: the splice exists to delete a frame, and a name resolution
costs far more than the frame it removed, so a non-zero `in_splice` means that
method's optimizing body is very likely slower than its single-pass one.

That row was non-zero for every spliced statically-bound call in the tree, and
until now there was no reading of any kind to see it by — which is how it
survived a measurement campaign that concluded the two bodies were
throughput-neutral.

## 5. What a spliced callee is still refused for

`putstatic`, `anewarray` / `multianewarray`, `athrow`,
`monitorenter` / `monitorexit`, `tableswitch` / `lookupswitch`,
`invokedynamic`, `jsr`/`ret`, integer division, and any body with more than one
`return` or a `return` that is not last.

`checkcast` / `instanceof` **came off this list on 2026-09-09** — see §8, which
is this section's prediction carried out and one of its claims corrected.

`putstatic` is the one that should stay refused until somebody does the barrier
work, not until somebody does the plumbing.

## 6. Regression check

`bench/CratonBench.java`, all seven phases, isolated process per phase, three
runs per arm, both switches off against both on. Every phase is inside 1% and
every checksum matches:

| phase | switches off | switches on |
|---|---|---|
| arithmetic | 4837 / 4851 / 4570 | 4879 / 4895 / 4594 |
| fib | 7905 / 7926 / 8164 | 7928 / 7965 / 8507 |
| sieve | 4199 / 4229 / 4197 | 4228 / 4202 / 4226 |
| matrix | 1925 / 1925 / 1906 | 1938 / 1915 / 1910 |
| hashmap | 7685 / 7680 / 7761 | 7625 / 7638 / 7585 |
| stringregex | 186 / 185 / 185 | 186 / 187 / 186 |
| bintrees | 9992 / 9980 / 10039 | 9882 / 9929 / 9869 |

That is the expected result and not a disappointing one: these phases are one
enormous loop per method and issue almost no optimizing-tier compiles at all.

`bench/CratonBenchC2.java` — the framework-shaped candidate — is likewise
unmoved (dispatch 305-358 vs 303-315, bind ~1693 both, pipeline ~152 both, all
checksums matching), and its refusal census says why: its callees are refused
for `native-shadow`, `ir-splice-target-not-provably-monomorphic` and
`checkcast/instanceof`, none of which either fix touches. That is the evidence
behind §5's claim that `checkcast` / `instanceof` is the next one to take.

The fast regression suite is 92 of 92 against HotSpot, and the crate suites are
2324 jit / 2639 vm / 606 types green.

## 7. What this does not claim

* **Not a general C2 win.** §6 measures exactly two workloads moving, and both
  are probes written for these two causes. Everything else measured is flat.
  What the fixes remove is a way for the optimizing tier to be an order of
  magnitude WORSE than the tier below it — which is a different and, on the
  evidence here, more valuable thing than a few percent.
* **The tier race is untouched and is now the visible oddity.** Under
  `CRATONVM_C2_ACCEPT=always` the optimizing body was installed in only 3 of 10
  runs of `SpliceStaticProbe`; the rest ran the single-pass body. That is why
  the fast mode appears in a minority of rows above. It was invisible while the
  optimizing body was the slow one and is worth its own investigation now that
  it is the fast one.
* **Not a fix for the acceptance gate.** `is_worth_publishing` still judges a
  body by which transforms RAN, not by whether the result is better, and the
  14x body it accepted here it accepted for a reason it still cannot see. What
  changes is that the specific way a body could be that much worse now has a
  census row instead of only a wall clock.
* **It spends inline budget, and that is not free.** Admitting `getstatic`
  makes the bodies along an accessor chain bigger — on
  `probes/StackTraceAfterOsr.java`, whose `leaf` reads a static array,
  `outer`'s optimizing body went 493 → 731 bytes — and the inline budget
  (`MAX_INLINE_BUDGET`, 750) then refuses a splice further out that used to
  fit. On that probe an OSR-compiled `main` stopped inlining `probe`, so `mid`
  and `outer` went from inline levels to ordinary interpreter frames. Nothing
  about the traces got worse (they still match the interpreter oracle exactly)
  and nothing measured got slower, but "more splicing at the bottom" is not
  monotone with "more inlining overall", and this is the first shape where that
  showed. It was found by
  `vm/tests/stack_trace_across_tiers.rs`, whose kill-switch arm went red
  because it had no inlined callee left to revert — see the comment there.
* **One host, one shape.** These numbers are from a Windows dev box, not the
  Azure bench host `BENCHMARK.md`'s table comes from, so the absolutes are not
  comparable to it. The A/B is: same binary, same window, alternated and
  order-flipped.

## 8. `checkcast` / `instanceof`, taken

§5 predicted this was the next refusal to take and that it was the same shape
as `getstatic`. It was, and it is done, behind
`CRATONVM_JIT_IR_SPLICE_TYPECHECK=1` — **opt-in, default OFF**, for a reason
§8.3 gives.

**One claim in §5 was wrong.** It said the arms want `(name_ptr, name_len)`
"into an arena this compile owns", and predicted a resolution step on top of
the rebase. They do not: `intern_typecheck_target` interns the name
PROCESS-WIDE and deliberately, because the type-check helpers memoize on the
`(ptr, len)` pair and it must not be recycled for another class when an
artifact is freed. So there was no arena lifetime to solve, and this is purely
a rebase — strictly less work than §5 budgeted for, not more.

### 8.1 What it is

`bench/SpliceCastProbe.java`. `unwrap` is `aload_0; checkcast #Box;
getfield #v; ireturn` — what `List<Box>.get(i).v` is after erasure — and
`tagOf` is the `instanceof` half. Both were refused whole at resolution.

The scanner's `0xc0 | 0xc1` arm now defers the site instead of refusing it,
resolving it against the CALLEE's constant pool exactly as `ir_new_sites` are
resolved, and `append_ir_inline_site` rebases the rows into
`IrInlineTables::checkcast_info` / `instanceof_info`. An UNRESOLVABLE site
refuses the whole body (`ir-splice-typecheck-unresolved`) rather than being
dropped: with `ir-unresolved-class-trap` off — its default — the builder's
`0xc0` arm bails the METHOD on a missing row, so admitting the body without it
would cost the caller its IR compile after the splice was committed to. Same
trade and same sentence as `ir-splice-static-field-unresolved`.

A spliced type check owes `has_dispatch`, for the same reason the caller's own
sites do: the helper resolves `&mut JvmThread` through `jit_thread_mut()`, and
the `!has_dispatch` fast entry never sets that TLS.

### 8.2 That it engages, and what it costs

Not inferred from a stopwatch — the refusal census says it directly:

```
TYPECHECK=0   [ir] inline-plan SpliceCastProbe.step(II)I:  (no spliced bodies)
TYPECHECK=1   [ir] inline-plan SpliceCastProbe.step(II)I: 2 site(s), 2 spliced
                                                          bodies, 21 bytes appended
              [ir] spliced 2 callee bodies into SpliceCastProbe.step(II)I
```

Both callees, and the `checkcast/instanceof` refusal for them disappears from
the IR-mode resolver while remaining for the single-pass one, which still has
no arm for either opcode.

Interleaved, order-flipped, 14 paired rounds, `CRATONVM_C2_ACCEPT=always`,
4 000 000 reps, Windows dev box, checksum `164255232` on every sample and
matching Temurin JDK 25:

| | median | |
|---|---|---|
| off | 209 ms | one mode |
| on | 184.5 ms | **−11.7 %, faster in 11 of 14 paired rounds** |

The ON arm has two modes and §7's tier race is why: 8 runs at ~180 ms (the
optimizing body with the splice installed) and 6 at ~207 ms (the single-pass
body, indistinguishable from the OFF arm). Against the OFF median the fast
mode alone is **−14.1 %**. Nothing here is a claim about the race, which §7
already flagged as untouched and which this work does not touch either.

### 8.3 The parity gap that never existed

Soak-clean: `tools/jit-flag-soak.sh` over the 39 deterministic workloads in
`vm/tests/resources/cratonvm`, under Generational, G1 and ZGC —
`divergent=0 nondeterministic=0` in all three.

**A spliced type check is the first spliced site that can THROW on a value the
caller produced.** `bench/SpliceCastThrow.java` drives the failing cast, the
null that must pass the cast and then fail the deref, and the `instanceof`
that must answer false, all from inside a relocated body. At 300 000 reps,
`TYPECHECK=0`, `TYPECHECK=1` and Temurin JDK 25 all return `-1589077594`.

An earlier revision of this section reported that CratonVM did **not** agree
with Temurin (`-1521462424`), called that an unexplained pre-existing
difference "independent of splicing", and held the switch off until somebody
chased it down. **That was this page's error, not the VM's.** The two arms
were run at different rep counts: the CratonVM arm passed `300000`, the
Temurin arm passed nothing and took `main`'s built-in default of `2_000_000`.
Both checksums are correct for the count that produced them. An independent
oracle that reimplements `step` in 32-bit arithmetic returns `-1589077594` at
300 000 reps and `-1521462424` at 2 000 000 — and Temurin re-run at 300 000
returns `-1589077594`, matching CratonVM exactly. There is no exception-path
divergence between this VM and HotSpot on this probe, and there never was one
to chase.

The lesson is narrower than the retraction: a checksum is only comparable
against another checksum taken at the same input size, and a probe whose
default rep count differs from the one the A/B passes will manufacture a
difference out of nothing. `SpliceCastThrow` now prints the count it used —

```
splicecastthrow (300000) [-1589077594]      <- CratonVM and Temurin JDK 25
splicecastthrow (2000000) [-1521462424]     <- the default, and the whole "gap"
```

— which is the convention `SpliceCastProbe` already had (`1. splicecast
(4000000) : ... ms [164255232]`), and is why §8.2's comparison was never at
risk while this one was.

### 8.4 Why it still ships off

With §8.3's blocker retracted, the honest question is whether anything is left.
§7 named one thing that could be: admitting more splices spends inline budget,
and on `probes/StackTraceAfterOsr.java` admitting `getstatic` pushed `outer`
from 493 to 731 bytes and cost an OSR-compiled `main` its inline of `probe`.
That is a real, measured, non-monotone effect — it is just not this flag's.
Asked of the type-check switch on the same probe, with
`CRATONVM_C2_ACCEPT=always` and `CRATONVM_DBG=jitc`:

| probe | `TYPECHECK=0` | `TYPECHECK=1` |
|---|---|---|
| `StackTraceAfterOsr` | 22 bodies, 198 B, 4 inline-planned | **identical** |
| `SpliceCastProbe` | 2 bodies, 64 B | 8 bodies, 127 B |

The budget-sensitive probe does not move at all; the probe built for this
feature is the one that splices more, which is the feature working. So the
§7 concern does not reproduce here.

That leaves no measurement arguing against the default — which is a weaker
position than the one this section used to hold, and it should be said
plainly rather than dressed back up. What remains is exposure, not evidence:
the path is one day old, and HotSpot parity for it rests on two probe shapes
(`SpliceCastProbe`'s `164255232` and `SpliceCastThrow`'s `-1589077594`) while
the 39-workload soak compares CratonVM against *itself* — flag on versus flag
off — which is self-consistency and not parity.

So the switch stays off for one cycle, on that reason and no other. Nothing
listed above needs re-deriving to flip it: it wants the suite green with the
flag on, in dev, for a cycle. If a later reader finds this section still
saying "off" with nothing new under it, the answer is to flip it, not to look
for a fresh justification — the last time this section held a switch off, the
reason turned out to be a rep-count mismatch nobody had re-checked in a day.

### 8.5 What is next, on the same evidence

`bench/CratonBenchC2.java`'s refusal census was §6's basis for naming
`checkcast`/`instanceof` next. Of the three reasons it listed —
`native-shadow`, `ir-splice-target-not-provably-monomorphic`, and
`checkcast/instanceof` — one is now gone. The other two are what a re-run of
that census should be read for; **`ir-splice-target-not-provably-monomorphic`
is the interesting one**, because unlike the three plumbing refusals taken so
far it is a real modelling question and not a rebase.
