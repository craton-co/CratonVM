# The third rebase: `checkcast` / `instanceof` in a spliced callee

Slug: `c2-splice-checkcast-and-instanceof` · 2026-09-09
The follow-up `c2-splice-getstatic-and-the-calls-it-left-behind-20260909.md`
§5 names, done the same way.

---

## What this is

The third instance of one pattern, and the last one that document identified:

| refusal | the reason given | what it actually was |
|---|---|---|
| `ir-splice-ldc` | the tag was dropped | plumbing — `InlineSite` lost the float/double bit |
| `ir-splice-static-field` | "no `static_field_info` is rebased" | plumbing — the rows existed, unrebased |
| `checkcast/instanceof` | (none; refused for both tiers together) | plumbing — the builder's arms existed, unfed |

`IrBuilder` has had `0xc0` and `0xc1` arms since cov-05, keyed by pc off
`checkcast_info` / `instanceof_info`. The splice scanner refused the shape for
both tiers in one arm, so the optimizing tier inherited a refusal that belongs
to the single-pass emitter, which genuinely has no arm for either.

That refusal fell on the commonest accessor in typed Java: every read out of an
untyped container is a `checkcast`. The survey that motivated the caller-side
arms counted **306 events on this pair — the largest single whole-method
refusal it found, more than every opcode gap combined.**

## What is different from the `getstatic` one

`getstatic` needed no resolution: `InlineSite::static_field_info` had carried
resolved rows for the single-pass inliner since it existed, and the work was to
rebase them. This one had no rows at all. `InlineSite` gains
`ir_typecheck_info`, and `resolve_inline_site_from` fills it by resolving each
site against the **callee's** constant pool — the only pool that can name the
target — for both the class id and the name.

Admission is the same bar the caller's own sites are held to: the target class
must be **already resolved and loaded**. The not-yet-loaded path runs
`jit_typecheck_resolve`, which can call a user classloader's `loadClass` —
arbitrary Java this tier does not host inside a helper call.

An unresolved target refuses the whole **callee**, which is `ir_new_info`'s
trade and not a new one: a missing row bails the METHOD, so admitting the body
without it costs the caller its optimizing compile over a callee it merely
wanted inlined.

**The `0xc0`/`0xc1` arms read as though they were gentler than that**, and this
is worth writing down because it cost a wrong first draft of both the code
comments and this file. A missing row reaches `plant_uncommon_trap`, not
`ir_build_bail`. But `TrapCause::UnresolvedTypeCheck` is gated behind
`ir_unresolved_class_trap_enabled`, which is **off** — the argument for it
having been refuted earlier — so the plant refuses and the arm bails after all.
With that gate on, the trap fires and, inside a splice, deopts to *re-execute
the invoke* on every call. Refusing at resolution is the answer that does not
depend on which way that flag is set, and the unit test asserts the bail
directly so that reading the arm alone cannot mislead the next person the way
it misled this one.

## The obligation that rides along

A `checkcast` obliges the artifact to carry `has_dispatch`. A definitive
refusal publishes its `ClassCastException` through the `JIT_THREAD` TLS, and
the `!has_dispatch` fast entry never sets that TLS. The caller-side feed sets
`ir_needs_dispatch_for_checkcast` for exactly this reason; a spliced
`checkcast` sets it too, merged only on a successful splice so a rolled-back
body cannot leave the artifact claiming a cast its code does not contain.

`instanceof` answers a boolean and owes nothing, which is why the two stay
separate maps rather than one.

`CRATONVM_JIT_IR_SPLICE_TYPECHECK=0` restores the refusal. Both halves read it.

---

## What it measures

`bench/SpliceCastProbe.java` — a hot loop reading a `List<Object>` through
`asBox` (a `checkcast`) and `kindOf` (an `instanceof`). The gate does what it
says: spliced bodies **2 → 6**, and the `asBox`/`kindOf` refusals go from four
each to one each.

The throughput result is smaller than the reach, and the reason is worth more
than the number.

### The gap on that probe is the container, not the casts

| arm | `SpliceCastProbe` |
|---|---|
| `C2_ACCEPT=never` (single-pass body) | 306-443 ms |
| `always`, `TYPECHECK=0` | 931-1065 ms |
| `always`, `TYPECHECK=1` | 947-970 ms |

The optimizing body is ~3x slower than the single-pass one **in both arms**, so
whatever that is, this lane does not cause it and does not fix it. The census
row added with the `getstatic` work names a suspect: `ir blind dispatches:
own_code=0 in_splice=1`, one surviving call inside a relocated body going
through `jit_invoke_dispatch`. It is an `invokevirtual` — `ArrayList.elementData`
— and the direct-bind fix deliberately covers only the statically-bound kinds,
because a virtual site is supposed to get the MIC/PIC cascade instead. It did
not get one here. **That is the next thing to look at, and it is a bug, not a
gap.**

`bench/SpliceCastArrayProbe.java` is the attribution: identical accessors,
identical loop, an `Object[]` in place of the `List`. There the optimizing body
is *not* slower — ~193 ms against the single-pass ~226 ms. The 3x follows the
container.

### On the array probe, the splice produces a faster body about a quarter of the time

34 interleaved order-flipped rounds, one binary, every sample
checksum-verified (`-931971200` throughout):

```
TYPECHECK=0:  168 173 179 182 189 190 191 191 191 192 192 193 193 193 194
              195 196 197 198 200 201 221 243  (min 168)
TYPECHECK=1:  129 130 130 133 134 135 138 188 189 189 190 191 192 193 193
              194 194 195 195 197 198 203 204  (min 129)
```

Medians are identical at 193 ms. What is not identical is that the ON arm has a
**second mode at ~133 ms that the OFF arm never reaches**, in 8 of 34 rounds,
and wins 22 of 34 paired rounds.

That mode is the spliced body; the 190s are the single-pass body. Which one a
run gets is the same tier race
`c2-splice-getstatic-and-the-calls-it-left-behind-20260909.md` §7 flagged as
"the visible oddity" — the optimizing body is installed before the timed loop in
only a minority of runs. So the honest summary is:

> The body this produces is ~30% faster than the body without it. On a run
> median the change is neutral, because the body is installed about a quarter
> of the time.

**Landed default ON** on that basis, matching its `ldc` / `branch` /
`getstatic` siblings: the capability is strictly additive, the body is strictly
better, and its value is gated on a race this lane does not own. A neutral
median is not a reason to ship the tier without the capability — it is a reason
to fix the race.

## Regression check

All seven `CratonBench` phases, three runs per arm, checksums matching and
timings flat (arithmetic 5948-6176 vs 6030-6478, fib 9270-10105 vs
9393-10181, sieve 5789-5817 vs 5779-5818, matrix 2602-2635 vs 2571-2609,
hashmap 10095-10261 vs 9955-10043, stringregex 229-245 vs 232-247, bintrees
12542-12772 vs 12566-12714 — a loaded box, which is why the absolutes are
above the earlier session's; the arms are what matter and they agree).

`CratonBenchC2` is flat too (bind 2344-2396 against 2300-2339, dispatch and
pipeline inside noise). The four `checkcast/instanceof` refusals still in its
census are the SINGLE-PASS ones: that emitter has no arm for either opcode, and
its refusal is untouched.

92 of 92 fast-regression vectors match HotSpot; 2330 jit, 2639 vm and 606 types
tests green.
