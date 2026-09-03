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

## The shape of the suspicion

`bytecode_walk.rs`, region `BOX_UNBOX`, inlines `Long.longValue()J` and
`Integer.intValue()I`. It pops the receiver off the simulated operand stack and
then dereferences it three times -- the class-id guard at `[RAX]`, the GC-flags
byte, and the payload load -- with no call and therefore no safepoint between
them. That is sound only while the receiver in hand cannot go stale. Under a
moving collector it evidently can.

**This is a hypothesis, not a finding.** What is measured is the pair of
switches above and the bisect. The next step is to keep the receiver as a NAMED
root across the sequence rather than only in `RAX`, and to re-enable the family
behind its own switch to check whether that closes it.

## The mitigation

`box_unbox_intrinsic_disabled()` now defaults to disabled. Set
`CRATONVM_JIT_BOX_UNBOX_INTRINSIC=1` to turn the family back on -- which is how
the root-cause work should run it. `CRATONVM_JIT_NO_BOX_UNBOX_INTRINSIC=1`
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
