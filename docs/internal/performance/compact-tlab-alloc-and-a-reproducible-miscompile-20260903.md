# The compact TLAB allocation shape, and a miscompile that now reproduces on demand

*2026-09-03. Branch `perf/compact-tlab-alloc-20260903`.*

## What this is for

Three fast paths in a row turned out to be dead because the object they were
gated on was not compact: the IR tier's inline `getfield` (fixed 2026-08-18),
the optimizing tier's gated reference store, and the single-pass one (both
2026-09-02). Each was fixed by emitting a second, legacy-shaped arm. The
common cause was never touched:

`init_object_header` — the interpreter's TLAB fast path — writes a LEGACY
header unconditionally, whatever compact layout the class has registered,
because it never consults `plan_object_alloc`.

It is worth being precise about who does what, because the earlier pages in
this series were not precise enough:

| allocator | shape |
|---|---|
| JIT inline `new` (`emit_inline_tlab_new`) | **compact**, with a layout-replace guard |
| `gen_heap::alloc_object` (TLAB miss, large objects) | **compact** |
| interpreter TLAB fast path | **legacy** |
| `jit_new_object` helper → TLAB | **legacy** (it routes through the above) |

So the same class already gets both shapes today depending on which allocator
ran, and every field access keys on the per-object `GC_FLAG_COMPACT` bit
precisely because of that. What is missing is not correctness but consistency:
the hottest allocation path is the one producing the shape every fast path
then has to have a second arm for.

## What this change does

`plan_tlab_object_shape` makes ONE shape decision per allocation and carries it
to the header stamp, so the reserved size and the stamped header cannot come
from two lookups that might disagree (the hazard the JIT's inline emitter
carries a layout-replace guard for). Every TLAB object site now uses it — the
interpreter's `gc_alloc_object`, both JIT-helper sites, the native-call site in
`vm_exec`, and the compact-String site in `vm_object` — which is what the
existing comment in `tlab_alloc_object_inner` demanded: *"If the two shapes are
ever unified it has to be done at every allocation site at once."*

The predicate is the same one the JIT's inline `new` uses — a registered layout
whose `field_count` matches this allocation exactly — plus a refusal unless the
process has a single `ClassStore`, since the interpreter's fast path has no
cheap route to the owning heap's layout domain and that is exactly the
condition under which the domain screen is vacuous.

A census comes with it, because a shape change with no count is unreadable:

```
[cratonvm] TLAB object shapes: compact=8538 legacy=6560 bytes-saved=210656
```

## FIXED, 2026-09-03 — the boxing fast path assumed a legacy allocation

The rest of this page is kept as written, because the reproduction is what
found the bug and the narrowing is what someone should copy next time. The
defect itself is now fixed.

**Root cause.** `jit_integer_value_of_direct` and its `Long` twin end their
TLAB fast path with a raw 16-byte `Value` cell written straight at
`HEADER_SIZE`, under a SAFETY comment asserting the object is legacy-layout:

> *"this arm JUST allocated `object` through the legacy TLAB path
> (`init_object_header`, zeroed 16-byte `Value` cells), so field 0 is the
> `Value` cell at `HEADER_SIZE`"*

That was an assumption about **which allocator this site calls**, not a
property of the object — and the cold arm three lines below it has always used
`set_field_as` precisely because *"this cold arm's allocator may pick a
non-legacy layout"*. With compact planning on, `java.lang.Integer` gets a
packed 4-byte `value` and the 16-byte cell overwrote it and the bytes after it.
`FjpProbe` sums boxed integers through a ForkJoinPool, which is exactly why it
was the probe that caught this.

Both sites now ask the object instead of assuming: `is_compact_object(header)`
picks `set_field_as`, and the raw store keeps the legacy path it was written
for. A grep for the same premise (`as *mut Value` near an allocation, and the
"legacy TLAB path" / "legacy-layout allocation" comments) finds no other holder
in the VM.

**A second, separate inconsistency fixed on the way.** `jit_new_object`
reserved a legacy-sized region while the TLAB wrapper re-planned the shape and
stamped a *compact* header onto it — an object claiming a smaller size than it
was given, which every header-strided heap walk then misparses. That is also
the reason ZGC alone survived: `note_tlab_object` records the true footprint in
ZGC's object-start registry, while `VmHeap::Generational | VmHeap::G1` discard
it and must rediscover the size from the header. The wrappers now DERIVE the
shape from the size actually reserved (`shape_of_reserved`) rather than
re-planning, so the two cannot disagree by construction.

**After the fix**, all three collectors agree with HotSpot:

| collector | `FjpProbe` with the switch ON |
|---|---|
| Generational | 499999500000 `OK`, compact=5024, 111,368 bytes saved |
| G1 | 499999500000 `OK`, compact=5024, 111,368 bytes saved |
| ZGC | 499999500000 `OK`, compact=6540, 123,496 bytes saved |

## Default ON since 2026-09-04

The soak the previous section asked for was run, and it is the reason the
default moved.

### The differential soak

231 programs from `apps/probes`, `probes/` and `bench/` compiled to 228
runnable classes, run **per collector** with a three-run protocol: twice with
the shape OFF, then once ON, comparing exit status and stdout byte for byte.

The two-run version of this was wrong twice before it was right, and both
mistakes are worth keeping:

1. `rc=$?` after a **pipeline** captured `tr`'s status, not the VM's. It was
   always 0, so rc divergence could never fire and a workload failing
   identically in both arms was compared on output instead of excluded.
2. Full stdout equality is the wrong oracle for a corpus that is mostly
   benchmarks: 33 of 228 "diverged" on wall times and self-tuned iteration
   counts. Masking timings by hand kept missing units (`ns/elem`) and can never
   fix a probe that varies its own line count.

The fix is the third run: **require the workload to agree with itself under a
fixed configuration before letting it testify about a change.** No list to
maintain, and a benchmark excludes itself by its own evidence.

| collector | agree | divergent | non-deterministic | failed with switch off |
|---|---|---|---|---|
| Generational | 164 | 2 | 41 | 21 |
| ZGC | 167 | 1 | 44 | 16 |
| G1 | 165 | 1 | 41 | 21 |

**496 deterministic program comparisons, zero semantic divergences.** All four
flagged cases were examined:

- `CpuClockCheck` (Gen) — a clock probe; `wall=500,0ms` against `wall=500,1ms`.
  It slipped the stability filter because two OFF runs happened to round alike.
- `TierOneArm` (ZGC) — `276 ns/op` against `401 ns/op`, with its actual result
  `sink=1` identical.
- `RandomLeak` (Gen and G1) — prints `javaHeapUsed`, and reports **14.7 MB
  against 9.6 MB, a 35% reduction**, for byte-identical program output. That is
  not a divergence, it is the change working.

### The rest of the gate

- `regression-suite/run.sh` **89/89 with the shape enabled** on the default
  collector, under `-XX:+UseGenerationalGC` and under `-XX:+UseG1GC`. This is a
  HotSpot-differential oracle, not a self-comparison, and it is the strongest
  evidence here.
- `FjpProbe` correct on all three collectors by default; `=0` restores the
  legacy shape and the census reads `compact=0`.
- Real applications from the H2 corpus: `TestCache` `rc=0` with **87.5 MB less
  allocated** (1,872,185 of 2,508,687 objects compacted); `TestIntPerfectHash`,
  `TestDataUtils` and `TestBitStream` all `rc=0`.
- Throughput: a wash, eight alternated rounds, medians 16.6 s either way.

### What is still not covered

Spring, Tomcat, netty and Keycloak are not in this soak — their runners
download fixtures this session could not fetch. The evidence above is 496
program comparisons, a HotSpot-differential suite on three collectors, and
four H2 applications. `CRATONVM_COMPACT_TLAB_ALLOC=0` restores the legacy
shape exactly and is the first thing to set if an object is ever suspected of
being read at the wrong offset.

## The case for OFF, as it stood before the soak

`CRATONVM_COMPACT_TLAB_ALLOC=1`. What the switch now has behind it:

- `regression-suite/run.sh` **88/88 with the switch ON** on the default
  collector, under `-XX:+UseGenerationalGC`, and under `-XX:+UseG1GC`.
- A real application correct with it on: `org.h2.test.unit.TestCache` `rc=0`,
  **1,872,185 of 2,508,687 objects compacted, 87,465,736 bytes saved**.
- `org.h2.test.unit.TestIntPerfectHash` `rc=0` on repeat, 96% of objects
  compacted.
- Throughput on that workload: a wash. Eight alternated rounds, on faster in 2,
  slower in 6, medians 16.6 s against 16.6 s. **The prize here is memory, not
  speed**, which is what `header-shrink.md` always said it would be.

What it does not have: any corpus beyond H2 and the regression suite. The class
of bug this exposed — code that assumes the TLAB hands back a legacy object —
was found by one probe touching one pair of sites, and Spring, Tomcat and netty
have not been run. Flipping the default deserves that soak and its own change.

## The original reproduction, and what it ruled out

The switch reproduced the defect deterministically, which is what made it
findable:

| collector | `FjpProbe` sum, switch off | switch on |
|---|---|---|
| Generational | 499999500000 `OK` | **215812748544** (no `OK`) |
| G1 | 499999500000 `OK` | **215812748544** (no `OK`) |
| ZGC | 499999500000 `OK` | 499999500000 `OK` |

HotSpot's answer is 499999500000. This is the same probe the comment in
`tlab_alloc_object_inner` names — *"a 2026-09-02 attempt … MISCOMPILED
`probes/FjpProbe.java`: wrong per-task sums, no collection involved"* — except
that comment attributes the attempt to the ZGC arm, and ZGC is the one
collector that now passes.

That turned a one-off anecdote into a lever anyone could pull, which is what
made the rest possible.

## How it was narrowed

Three experiments, each a single run:

1. **It is not in the JIT.** `CRATONVM_NO_JIT=1` with the switch on still
   produces a wrong sum (217283218880). Every compiled-code emitter — the
   gated reference stores, the inline `getfield`, the inline `new` — is
   therefore off the list.
2. **It is not the atomic intrinsics.** `CRATONVM_JIT_NO_ATOMIC_INTRINSIC=1`,
   `CRATONVM_JIT_NO_ATOMIC_LONG_INTRINSIC=1`, and both together all reproduce
   it unchanged — worth checking because `AtomicInt`/`AtomicLongFieldLayout`
   are the two places that bake a compact AND a legacy address and pick per
   object.
3. **It is collector-dependent, and the difference is object registration.**
   ZGC passes; Generational and G1 fail. The one structural difference on this
   path is `note_tlab_object`: ZGC records the TLAB object and its footprint in
   its object-start registry, while `VmHeap::Generational | VmHeap::G1` discard
   both arguments. Those two collectors therefore have to rediscover a TLAB
   object's size by walking and reading its header — which is exactly the thing
   this change alters.

That lead was half right: `note_tlab_object` is exactly why ZGC alone survived
the *size* inconsistency described above. It was not the whole story — the
value-corrupting half was the boxing fast path, which no collector-shaped
hypothesis would have reached.

What actually closed it was a fourth step the first three made cheap: a
per-site bitmask (`CRATONVM_COMPACT_TLAB_SITES`) and a per-class dump
(`CRATONVM_DBG_COMPACT_TLAB=1`), one build, then bisection with no further
builds at all. Five sites, five runs:

| `CRATONVM_COMPACT_TLAB_SITES` | site | `FjpProbe` |
|---|---|---|
| 1 | interpreter `new` | correct |
| 2 | `jit_new_object` | correct |
| **4** | **the JIT boxing helpers** | **215812748544** |
| 8 | native calls | correct |
| 16 | compact `String` | correct |

Both levers are kept. A shape change that goes wrong again will be one build
and five runs from an answer.

## Status

- The miscompile is FIXED; `CRATONVM_COMPACT_TLAB_ALLOC=1` now produces
  HotSpot's answer on all three collectors.
- Default still OFF, pending a soak beyond H2 and the regression suite.
- With the switch off, every path is byte-for-byte the behaviour it had before.
- Levers: `CRATONVM_COMPACT_TLAB_ALLOC=1` (the shape),
  `CRATONVM_COMPACT_TLAB_SITES=<mask>` (bisect by site),
  `CRATONVM_DBG_COMPACT_TLAB=1` (which classes go compact).

## The prize, for whoever fixes it

On `FjpProbe` alone the switch converts 8,538 of 15,098 TLAB objects to the
compact shape and saves 210,656 bytes — ~14 KB per thousand objects, on a
probe that is not allocation-heavy by this repo's standards. `header-shrink.md`
puts `HashMap.Node` at 72 bytes compact against 96 legacy. And the three
second-arm fast paths this series added exist only because the common case is
the shape this switch would retire.
