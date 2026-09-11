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

> **Corrected 2026-09-10.** The identification in this paragraph is wrong on
> every count, and the section at the end of this page has the measured
> version: the surviving call is `Preconditions.checkIndex`, it is
> `invokestatic` and so is not entitled to a MIC/PIC at all, and it is blind
> because it is native-shadowed and the direct-bind path declines those.
> Fixing it made the optimizing body 2.25x faster on this probe. The paragraph
> is left standing because the wrong guess is the reason the diagnostic that
> found the right answer exists.

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

---

## The blind dispatch, taken — and it was not what this page said it was

**2026-09-10.** The section above closed on `ir blind dispatches:
own_code=0 in_splice=1`, named a virtual `ArrayList.elementData` as the
surviving call "that got neither a direct bind nor the MIC/PIC cascade it
should have", and called it the next thing to look at. It was the next thing
to look at. It was not that call, not that kind, and not that remedy.

The count was the only evidence there was, so the first step was to make the
lowerer say which site it is. `CRATONVM_DBG=jitc` now prints one line per blind
dispatch naming the target and the state of every gate that could have routed
it elsewhere:

```
[ir] blind-dispatch pc=18 in_splice=true
     jdk/internal/util/Preconditions.checkIndex(IILjava/util/function/BiFunction;)I
     kind=3 num_args=3 ic_slot=none direct_row=false
     mic_helper=true abi_regs=4 direct_calls_gate=true
```

`kind=3` is `invokestatic`. A statically bound call is not supposed to get a
MIC/PIC — the cascade selects on a runtime receiver and there is not one — so
"the MIC/PIC cascade it should have" was the wrong remedy for a site that was
never entitled to one. `mic_helper`, `abi_regs` and `direct_calls_gate` say the
inline-cache machinery was available and unblocked; it simply does not apply.

What the site was missing is a **direct bind**, and the compiler's own log says
why it has none:

```
bg-direct-call BOUND    java/util/Objects.checkIndex(II)I
bg-direct-call DECLINED jdk/internal/util/Preconditions.checkIndex(...): native-shadow
inline-resolve REFUSED  jdk/internal/util/Preconditions.checkIndex(...): native-shadow
```

`Objects.checkIndex(II)I` is compiled and direct-bound. It is then spliced into
`ArrayList.get`, whose code is 15 bytes, at the call `ArrayList.get` makes to
it; the call that spliced body leaves behind sits at callee pc 3, so combined
pc **18** — the pc in the diagnostic. That surviving call is
`Preconditions.checkIndex`, which is **native-shadowed**, and the direct-bind
path declines a native shadow.

So the splice replaced a bound direct `CALL` (~4 ns) with a blind name
resolution (~175 ns), per element, on the hot path.

### That trade already had a rule against it

The single-pass resolver refuses a splice on exactly this ground: *a spliced
call must not be worse than the call it replaced*. IR mode was exempted from
it, on the stated grounds that "the IR tier's spliced call is a call, not a
resolution" (`c2-splice-getstatic-and-the-calls-it-left-behind-20260909.md`
§3). That premise holds whenever a surviving call can be lowered to something
better than the helper, and there is one shape where it cannot: a statically
bound call with no `direct_entry` has no cache to fall back on. Virtual and
interface survivors are fine — the cascade needs no plan-time binding.

`append_ir_inline_site` now refuses a site whose body leaves such a call,
before `intern_inline_invoke_targets`, so a refused site also registers no
keep-alive entry for a callee the artifact will not call.
`CRATONVM_JIT_IR_SPLICE_REFUSE_UNBINDABLE=0` re-admits the trade, for measuring
it rather than arguing about it.

### What it is worth

`bench/SpliceCastProbe.java`, 4 000 000 reps, `C2_ACCEPT=always`, five
interleaved rounds, checksum `-931971200` on all fifteen samples:

| arm | samples (ms) | median |
|---|---|---|
| optimizing, **refusal on** | 493 472 539 581 477 | **493** |
| optimizing, refusal off | 1074 1112 1195 1091 1129 | 1112 |
| single-pass (`C2_ACCEPT=never`) | 344 335 338 370 318 | 338 |

**2.25x faster, won in 5 of 5 paired rounds, with no overlap between the two
distributions at all.** Under the default `evidence` policy, which is what
production runs, it is **2.84x** — see "The number that was hiding behind it"
below for why the forced-acceptance arm UNDERSTATES this fix. The census row this page ended on goes to
`own_code=0 in_splice=0`, and `step` still gets its 3 spliced bodies — the
refusal lands on `ArrayList.get`'s compile, not on the type-check splices this
page is about.

`bench/SpliceCastArrayProbe.java` is unchanged, as it must be: 206/145/161 ms
with the refusal against 199/211/206 without. It has no unbindable survivor,
which is the same reason it was the right attribution probe in the first place.

### What is left, and where it actually was

The 1.46x above is real, and it is not in this lane, in `step`, or in the
type-check splices. It is an artifact of the harness setting used to defeat the
tier race — and chasing it turned up the more important number.

**Isolation.** Four variants, same accessors and loop, `C2_ACCEPT` forced both
ways, checksums identical within each:

| variant | the read is | `always` | `never` | ratio |
|---|---|---|---|---|
| `SpliceCastProbe` | `List.get` (invokeinterface) | 469 492 473 | 362 378 305 | 1.32x |
| V2 | `ArrayList.get` (invokevirtual) | 481 519 492 | 339 344 | 1.44x |
| V4 | `ArrayList.get`, **no casts at all** | 256 239 240 | 115 108 108 | **2.2x** |
| V3 | bare `Object[]` load | 225 211 207 | 208 242 208 | 1.0x |
| V5 | a USER container: bounds check + array load | 69 71 69 | 79 74 63 | **0.96x** |

V2 says it is not interface dispatch — invokevirtual has the same gap. V4 says
it is not the casts — strip them entirely and the gap grows. V5 says it is not
the *shape*: a user-written container with `get`'s exact structure is FASTER
under the optimizing tier. V4 against V5 is the finding: 240 ms against 69 ms
for the same work, so the cost is `ArrayList.get` specifically.

**What it is.** `Objects.checkIndex(II)I` is one line — `return
Preconditions.checkIndex(index, length, null)`. Its single-pass body is 277
bytes. Its optimizing body is **694**. `ArrayList.get`'s are 2672 and 2770.
`C2_ACCEPT=always` forces those bodies onto JDK internals where the optimizing
tier's fixed overhead is a straight loss — the same family as this page's
sibling records for statics behind accessors. Under the DEFAULT `evidence`
policy both are refused (`acceptance ...: REFUSED (evidence: none) -- keeping
the single-pass body`), so no production run ever sees them.

So the 1.46x is a property of the lever, not of the tier. Which is worth saying
plainly: **`C2_ACCEPT=always` is not "the optimizing tier's number". It is the
optimizing tier with its acceptance gate removed, including on JDK code where
that gate is the only thing standing between a program and a slower body.**

### The number that was hiding behind it

Re-run under the default policy — the one production uses — and the
unbindable-call refusal is worth more, not less:

| default (`evidence`) policy | samples (ms) | median |
|---|---|---|
| refusal on | 320 307 331 312 | **316** |
| refusal off | 898 893 954 897 | **897** |

**2.84x, 4 of 4 paired rounds, no overlap — and 316 ms is below the single-pass
338 ms**, so with the refusal in place the optimizing tier is no longer behind
on this probe at all.

The mechanism is the part worth keeping:

```
refusal off: [ir] inline-plan java/util/ArrayList.get: 1 site(s), 1 spliced body, 7 bytes appended
             ... and no acceptance line: the body is PUBLISHED
refusal on:  [ir] acceptance java/util/ArrayList.get: REFUSED (evidence: none)
             -- keeping the single-pass body
```

Splicing `Objects.checkIndex` into `ArrayList.get` was the only transform in
that compile, and `is_worth_publishing` reads "a transform happened" as
evidence. **So the splice was the evidence that got the slower body published.**
The harmful transform paid for its own admission. Refusing it removes the
transform, which removes the false evidence, which leaves the single-pass body
in place — three effects from one refusal, and only the first was intended.

That is the failure mode `c2-splice-getstatic-and-the-calls-it-left-behind-20260909.md`
§7 named and declined to fix: "`is_worth_publishing` still judges a body by
whether it changed, not by whether it helped." This is that gate mis-firing in
production, on JDK container code, on the default policy — not a lab artifact.
Fixing the gate to judge by measurement is still not done here, and is still
the more valuable repair; what is done is removing one transform that was
lying to it.
