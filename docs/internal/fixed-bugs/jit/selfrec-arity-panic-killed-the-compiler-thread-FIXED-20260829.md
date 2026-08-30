# A self-recursive call with 4+ args panicked the compiler thread — and it does not come back

**FIXED 2026-08-29.** Found while re-verifying an unrelated ZGC fragmentation
page: one of its occurrences could not reach its own failure because of this.
The fix, the gate and the blast-radius census are all here.

## The defect

`ir_lower::emit_self_recursive_call` marshals the hidden VM context pointer plus
**every** Java argument into an entry-ABI register — `abi[0]` for the context,
`abi[1 + i]` for argument `i`. The file is **four** registers on Win64
(RCX, RDX, R8, R9) and **six** on SysV. Nothing checked the arity, so a
`static long f(int,int,int,int)` calling itself indexes `abi[4]`:

```text
thread 'cratonvm-jit-compiler' panicked at jit/src/ir_lower.rs:3282:38:
index out of bounds: the len is 4 but the index is 4
```

## Why it survived: it is invisible from Java, twice over

The panic is on the **background compiler thread**. The program's answers stay
correct, the VM does not die, and nothing is printed that a test framework
reads. What actually happens is worse than a lost compile — **the thread does
not come back.**

MEASURED with `CRATONVM_DBG_JIT_COMPILED=1` on `probes/SelfRecArgs.java`, an
arity sweep f1..f6:

```text
put SelfRecArgs.f1(I)J
put SelfRecArgs.f2(II)J
put SelfRecArgs.f3(III)J
thread 'cratonvm-jit-compiler' panicked … index out of bounds: the len is 4 …
    (nothing after this — not f4, not f5, not f6, not main's OSR)
CK selfrec args=1..6 ok=true        ← every answer still correct
```

**One such method anywhere in a workload silently drops the whole process to
the interpreter for the rest of its life**, with correct answers throughout. It
reached a real workload as a hang: Hibernate's
`boot.database.qualfiedTableNaming.DefaultCatalogAndSchemaTest` panicked during
bootstrap and ran to a 600 s timeout interpreted, producing no `@@RESULT` and no
GC lines — which, in a fragmentation investigation, read as "the fragmentation
is gone".

## The guarantee was real, and then it was removed

`lower()` used to refuse outright any graph with more incoming slots than
registers, which is exactly what made the marshal's assumption true. The comment
at the marshal still cited it:

> `1 + num_args <= abi.len()` is guaranteed by the needs_context bail in
> `lower()`, so no arg spills off the register file.

When `emit_prologue` learned to read the overflow off the caller's stack
(Gap B), that bail went away — deliberately, and for a good reason: it was *"the
commonest whole-method refusal an ordinary accessor hits"*. The self-call
marshal was not part of that change and kept asserting a guarantee nobody was
maintaining any more.

`incoming_abi_reg_capacity`'s own doc comment had already written the warning:
*"Kept because the prologue still has to know where the register half stops.
Historically `lower()` refused such a graph up front — see the Gap B bail there
for what that cost the last time the two got out of step."* This is the other
side of the same sentence. **A premise in a comment is not a compile-time
link** — the same lesson `jit_aastore`'s note records for a different pair.

## The fix

Enforced at **eligibility**, not at the marshal, because eligibility is the only
place that can still choose a different route. `is_self_recursive_direct` now
also requires `desc_args + 1 <= ir_lower::incoming_abi_reg_capacity()`, and a
self-recursive **wide-return** method that does not fit takes the same exit as
`!selfrec_direct` — single-pass, which emits its own direct self-call — rather
than `jit_invoke_dispatch`, the route the fib44 note measured at 8.6x slower.

Gate: `jit/tests/ir_vs_singlepass.rs::selfrec_more_args_than_entry_regs_still_compiles`,
at arities 4 **and** 6 so the test is not vacuous on a six-register host.
Falsified by removing the guard (panics) and restoring it (passes); the whole
`cratonvm-jit` crate is 2381 tests green.

## Blast radius, measured

A census line was added at the refusal, under the existing
`CRATONVM_DBG_JIT_COMPILED` flag rather than a new one — the refusal is
invisible from Java, so without it "what does this guard turn away" has no
answer.

**Hibernate ORM, first 150 classes of `passed.txt`:**

| | |
|---|---|
| classes with ≥1 refusal | **4 / 150 (2.7 %)** |
| distinct refused methods | **5** |

```text
java/util/Arrays.mergeSort([Ljava/lang/Object;[Ljava/lang/Object;III)V                 args=5
java/util/Arrays.mergeSort([Ljava/lang/Object;[Ljava/lang/Object;IIILjava/util/Comparator;)V args=6
com/sun/org/apache/xerces/…/XSConstraints.checkElementDeclsConsistent(…)V              args=4
org/h2/util/Utils.partialQuickSort([Ljava/lang/Object;IILjava/util/Comparator;II)V     args=6
org/hibernate/engine/internal/ForeignKeys.collectNonNullableTransientEntities(…)V      args=7
```

Classes: `LocalXmlResourceResolverTest`, `BatchSizeExceedingTest`,
`CompositeIdTest`, `ConfigurationTest`.

**Two of the five are in the JDK itself**, which is what makes this general
rather than a Hibernate story: `java.util.Arrays.mergeSort` is the legacy object
sort, and the Xerces one is XML-schema validation. Any workload that reaches
either loses its JIT from that point on.

**Regression suite: 0 refusals across all 78 vectors.** The corpus contains no
compiled self-recursive method wide enough to trip it, so it could never have
caught this — which is why the repair ships with a probe and a unit test rather
than a suite vector.

## A measurement trap found on the way

`CRATONVM_DBG_JIT_COMPILED=1` is **not a neutral configuration for the
regression suite**. Running `SUITE=core` with it reports 76/78 with
`RSimpleTimeZoneRaw` and `RSslLiveSession` red; the same binary without it is
78/78, and re-running those two with the flag reproduces
`RSimpleTimeZoneRaw FAIL output differs from HotSpot` while its own body still
prints `PASS RSimpleTimeZoneRaw (393 checks)`. The census line cannot be
involved — it emits nothing in that run, the refusal count being 0. Read a
flagged run for its census output, never for its verdict.
