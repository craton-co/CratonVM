# `LongLongHashMapTest.randomOperations` NPE — FIXED 2026-08-28: a spliced constructor's `this` was not a rewritable GC root

## Status

**FIXED.** `fix/netty-longlong-npe-and-compression-wall-20260827`, commit
`b68e37d63`. `LongLongHashMapTest` 18/20 runs failing → **0/20**; the AssertJ
repro `AjProbe2` ~18/20 → **0/20**; `regression-suite/run.sh` 72/72.

Superseded page:
`known-issues/netty/longlonghashmaptest-nullpointerexception-not-in-the-class-under-test-20260827.md`.

## What the page got right, and the one thing it did not

The page was right that `LongLongHashMap` cannot produce a
`NullPointerException` — it is a pure `long[]`-backed open-addressing map with
no reference in its hot path — and right that the NPE was therefore CratonVM's
own execution. It nominated two candidates: the JIT's compilation of the hot
loop, or AssertJ's overload-resolution machinery. It was the first, and neither
guess named the mechanism, because the defect is not in either party's code.

The reported line is `LongLongHashMapTest.java:80`, whose bytecode (bci 212-228)
is `aload actual; get(J)J; assertThat(J); ldc -1; isEqualTo(J)`. A probe that
checked every subexpression showed the receiver of `isEqualTo` was NOT null —
and then NPE'd anyway. Dumping the object's fields named the real null:

```
=== NPE at L73 i=1627 arg=-1
  assert obj = org.assertj.core.api.LongAssert
    AbstractLongAssert.longs (Longs) = NULL              <-- assigned unconditionally
    AbstractComparableAssert.comparables (Comparables) = Comparables [...]
    AbstractAssert.objects (Objects) = org.assertj.core.internal.Objects@1172
    AbstractAssert.actual (Object) = -1
    AbstractAssert.myself (AbstractAssert) = org.assertj.core.api.LongAssert@1
```

`AbstractLongAssert` assigns `longs = Longs.instance()` in its constructor, with
no branch. Every OTHER field of the object — including `comparables`, assigned
one constructor earlier — is intact. So the store had been **lost**, and the one
that was lost was the LAST one performed in the whole construction.

## Bisect

Eight runs per arm, one binary except where noted:

| arm | failures |
|---|---:|
| base | 8/8 |
| `--nojit` | 0/5 |
| `-XX:+UseG1GC` | 0/8 |
| `CRATONVM_ZGC_RELOCATE=0` | 0/8 |
| `CRATONVM_ZGC_RELOCATE_UNDER_PROVEN_JIT=0` | 0/8 |
| `CRATONVM_JIT_DENY=AbstractLongAssert` | 0/8 |
| `CRATONVM_JIT_INLINE_CALLS=0` | 0/8 |
| `--Xmx 8g` (ZGC) | 4/8 |
| `--Xmx 64m` (ZGC) | 7/8 |
| every other JIT lever tried (inline-new, TLAB-new, inline-putfield, ctor-direct-call, field-site-cache, scalar-replacement, precise-field-ops, conservative-locals, precise-maps, callee-oop-flush, ZGC generational) | 4-8/8, i.e. no signal |

A JIT defect, needing a MOVING collection, in the compiled
`AbstractLongAssert.<init>`, and requiring calls inside spliced bodies.

## The instrument that named it

`CRATONVM_DBG=remap-residue` (added by the fix) walks a JIT frame's whole band
AFTER its oop map has been applied and reports every word still pointing into
from-space, with the method it belongs to. One run:

```
[remap-frame] method=org/assertj/core/api/LongAssert.<init>:(Ljava/lang/Long;)V
              sp_id=4 ... stale_words=12
```

and `AbstractLongAssert.<init>` absent from the walked chain entirely — because
it had been SPLICED into `LongAssert.<init>`, whose frame is the one holding
twelve stale words.

## Root cause

A call-carrying inline splice (`CRATONVM_JIT_INLINE_CALLS`, default-on since
2026-08-20) puts the spliced callee's JVM locals in the CALLER's spill area and
then emits real, GC-capable calls from inside that body. Nothing named those
slots: `local_oop_masks` describes the ENCLOSING method's locals and the operand
marks describe operands.

`emit_inline_direct_call` says so, and argues it is safe:

> the callee locals this splice reserved live in the frame's spill area and are
> covered by the same conservative frame sweep as every other spill slot —
> over-approximate, hence pinned by a moving collector.

That premise stopped holding on 2026-08-21, when ZGC's `relocate_stw` gained its
relocate-under-proven-JIT path: when the per-cycle coverage proof passes, the
conservative scan is SUPPRESSED, so those slots are neither pinned nor
rewritten. And the proof consulted a completeness claim
(`moving_young_safepoint_coverage_complete`) that had never looked at splices at
all — so it claimed complete coverage of a frame it could not describe.

Concretely: `AbstractLongAssert.<init>`, spliced into `LongAssert.<init>`, holds
`this` in a callee local across its super-constructor call. The slide moves the
`LongAssert`. The splice's trailing `putfield longs` lands on the vacated copy.
The caller's own reference was rewritten, so the object that survives is the one
with every field but the last.

Two changes made independently, each sound in isolation, and the composition
silently unsound. Neither was wrong about the collector it was written against.

## The fix

`Compiler::inline_oop_scopes` gives a splice a scope. It runs the SAME forward
"must be oop" dataflow (`compute_local_oop_masks`) over the callee's own
bytecode, seeded with the callee's reference parameters, and three places
consult it:

* `emit_oop_map_for_safepoint` Stage 3b names the live callee-local slots, so
  `remap_active_jit_frames` rewrites them;
* `collect_live_oop_homes` publishes them on the shadow stack, which is the
  channel a parked peer remaps itself through and the only one the band verifier
  consults;
* `moving_young_safepoint_coverage_complete` REFUSES the completeness claim when
  a live scope cannot classify its locals (>64 locals, an unreached pc), so an
  undescribable splice takes the non-moving sweep instead of a wrong claim.

A second defect, found by the same instrument and fixed with it:
`remap_one_jit_frame` looked up the frame's active map with `find`, but
`bytecode_pc` is the ENCLOSING invoke's bci and a splice emits one safepoint per
`invoke*` in the body — two maps under one id with different live sets. It now
takes the UNION over `filter`, which is what every other reader of that table
(`moving_young_frame_coverage_complete_at`, the band verifier) already did.

## Why the band verifier did not catch it

It looked. `band_slot_is_verifiable_with_map` skips a word in a
dataflow-modelled region that the active map does not name — the repair for the
"verifier refuses on a DEAD word" half of the H2 `TestKillProcessWhileWriting`
chain. An inlined callee's local is exactly such a word: modelled region, not in
the map, and very much alive. The fix closes the hole from the other side, by
putting those slots IN the map.

## Scope

This is not a netty defect and not an AssertJ one. Any moving-GC cycle that
lands while a call-carrying splice is on the stack could lose a reference the
splice held in a callee local; a constructor is simply the shape where the loss
is most visible, because the object is published and the missing field reads
back null. `LongLongHashMapTest` is one instance, found because AssertJ builds a
fresh five-constructor-deep assert object 300 000 times in a loop.

## Related

* `fixed-suite-bugs/netty/unexplained-npes-in-randomized-tests-CLOSED-20260829.md`
  — the original report, retired 2026-08-29 once its other two classes were
  cleared under load.
* `bug-h2-testkillprocess-zgc-oom-at-97-percent-free-20260821.md` — the chain
  that introduced relocate-under-proven-JIT and the band verifier's map-liveness
  screen.
