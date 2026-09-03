# The box/unbox intrinsic SIGSEGVs under a relocating collector

## Status

**OPEN (root cause), MITIGATED (default flipped) 2026-09-02.**
`CRATONVM_JIT_BOX_UNBOX_INTRINSIC` is now opt-in. The crash it causes is gone
from the shipped default; the reason the inline sequence is unsafe under a
moving collector is NOT yet established, and that is what stays open.

## What happens

`org.h2.test.store.TestRandomMapOps --Xmx 256m`, shipped default, quiet host:
**SIGSEGV in 25-183 s, 11 runs out of 11.** The fault address is always a page
boundary (`addr=0x...f0000`, `rdi` equal to it, fault pc inside libc) which is
what a read through a reference into a page the collector has already vacated
looks like. The in-process report is truncated by design -- it needs the
allocator, which is not safe in a signal handler -- so the Java frame is not in
it.

## Two switches each remove it

Same binary, three runs of 1200 s per arm, nothing else on the host:

| arm | SIGSEGV |
|---|---|
| shipped default | **11 / 11** (25-183 s) |
| `CRATONVM_ZGC_RELOCATE=0` | 0 / 3 |
| `CRATONVM_JIT_NO_BOX_UNBOX_INTRINSIC=1` | 0 / 3 |

So it takes BOTH relocation and this intrinsic. Neither alone is enough, which
is why it did not show up in the perf work that landed the intrinsic.

## The bisect, and why it lands on a merge

`git bisect` over the 200 commits between the last known-good tip
(`777688aa5`) and the crashing one (`120bb7c37`):

* both endpoints were re-verified in the **same build profile** first -- the
  known-good binary had been a release build and the crashing one `livedbg`,
  and a cross-profile comparison is not a bisect endpoint;
* only `SIGSEGV` counted as BAD. The `NullPointerException` and the
  fragmentation `OutOfMemoryError` this workload also produces both PRE-DATE
  the range (see
  `known-issues/h2/bug-h2-testrandommapops-small-heap-corruption-20260829.md`),
  and counting them would have bisected to the wrong defect. One commit in the
  range failed twice with `rc=1` and was still, correctly, scored GOOD.

First bad commit: **`a910b7d9c`** -- a MERGE whose two parents are BOTH good.
Its relocation files are byte-identical to parent 2, so nothing was
hand-resolved there. The defect is the INTERACTION between
`perf/box-random-intrinsics-20260902` and dev's relocation, not either alone.

## Narrowed 2026-09-02 (later): it is relocation UNDER LIVE JIT FRAMES

Four more arms on the pre-fix binary (intrinsic default-ON), 1200 s cap, quiet
host, each with the default arm re-run in the SAME batch as a positive control
so a quiet batch cannot be mistaken for a fix:

| arm | SIGSEGV |
|---|---|
| default (positive control) | **2 / 2** (34 s, 121 s) |
| `CRATONVM_COMPACT_REF_FIELDS=0` | **3 / 3** — hypothesis REFUTED |
| `CRATONVM_ZGC_RELOCATE=0` | 0 / 3 |
| `CRATONVM_ZGC_RELOCATE_UNDER_PROVEN_JIT=0` | **0 / 3** |

**Refuted: the compact/legacy offset fallback.** `AtomicLongFieldLayout::new`
falls back to `compact = legacy` when `compact_field_storage` has no entry, and
the emitted code still branches on `GC_FLAG_COMPACT` and uses `compact` for a
compact instance — so a compact object read at a legacy offset can land past
its end, which would fault exactly like this. It is a real smell and it is NOT
this defect: with compact fields off entirely, all three runs still SIGSEGV.
Written down because it looked convincing and cost one run to rule out.

**Confirmed: the narrow switch is enough.** `RELOCATE_UNDER_PROVEN_JIT=0` keeps
compaction on everywhere EXCEPT under live compiled frames, and that alone
removes the crash. So the stale value is a reference held in a LIVE JIT FRAME
that relocation moved without rewriting — a root the safepoint's oop map does
not name.

That is the same family as
`known-issues/h2/bug-h2-testrandommapops-small-heap-corruption-20260829.md`,
which has been hunting an unnamed root in a compiled frame for days and whose
oracle reports `local_oop=0`. This is a fresh, cheap, 100%-reproducible
instance of that shape — and unlike that page's witness, this one has a switch
that turns it on and off in one binary.

## The shape of the suspicion

`bytecode_walk.rs`, region `BOX_UNBOX`, inlines `Long.longValue()J` and
`Integer.intValue()I`. It pops the receiver off the simulated operand stack and
then dereferences it three times -- the class-id guard at `[RAX]`, the GC-flags
byte, and the payload load -- with no call and therefore no safepoint between
them. That is sound only while the receiver in hand cannot go stale. Under a
moving collector it evidently can.

**This is a hypothesis, not a finding.** What is measured is the switch table
above and the bisect. The next step is to keep the receiver as a NAMED root
across the sequence rather than only in `RAX`, and to re-enable the family
behind its own switch to check whether that closes it.

Note what the sequence does to the receiver's SLOT: `pop_stack()` releases it,
and `push_from_rax()` can hand the same spill slot straight back for the
primitive result. Between those two the reference lives only in `RAX`. Any
safepoint that observes the frame in that window sees a slot the map no longer
names — or, worse, names as holding the primitive that replaced it.

## The mitigation

`box_unbox_intrinsic_disabled()` now defaults to disabled. Set
`CRATONVM_JIT_BOX_UNBOX_INTRINSIC=1` to turn the family back on -- which is how
the root-cause work should run it. The fix arm was verified at **3 of 3 runs
clean to the 1200 s cap** on the workload that crashed 11 out of 11. `CRATONVM_JIT_NO_BOX_UNBOX_INTRINSIC=1`
still forces it off and still means the same thing, so any script that already
sets it is unaffected.

Correctness first: the measured speedup is recoverable the moment the sequence
is made relocation-safe.

## Reproducing

```
cargo build --profile livedbg -p cratonvm-cli
CRATONVM_JIT_BOX_UNBOX_INTRINSIC=1 cratonvm --java-home $JDK25 --Xmx 256m \
  -c "$H2_CP" org.h2.test.store.TestRandomMapOps
```

Under three minutes on a quiet host. A CONTENDED host hides it -- the same
lever this repo has been bitten by before.
