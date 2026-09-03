# The box/unbox intrinsic SIGSEGVs under a relocating collector

## Status

**OPEN (root cause), MITIGATED (default flipped) 2026-09-02.** The stated
hypothesis was refuted on 2026-09-02 -- see below -- and the search is narrowed
rather than closed.
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

## The shape of the suspicion -- and why it cannot be right as stated

`bytecode_walk.rs`, region `BOX_UNBOX`, inlines `Long.longValue()J` and
`Integer.intValue()I`. It pops the receiver off the simulated operand stack and
then dereferences it three times -- the class-id guard at `[RAX]`, the GC-flags
byte, and the payload load -- with no call and therefore no safepoint between
them.

This page originally read: *"That is sound only while the receiver in hand
cannot go stale. Under a moving collector it evidently can."* **That cannot be
the mechanism.** Relocation here is stop-the-world -- `ZgcRealHeap::relocate_stw`
-- so the mutator is parked at a safepoint while objects move. A receiver held
in a register across a stretch containing NO safepoint is not the unsafe case;
it is precisely the safe one. Nothing can move under that sequence.

So the receiver must already be stale when it is LOADED. The question is not
"what moves it while we hold it" but **"why was the slot it came from not
healed at the last relocating safepoint"** -- which is a question about the oop
map, not about the length of the inline sequence.

That also retires the next step this page used to propose. Keeping the receiver
as a named root ACROSS the sequence fixes nothing under STW relocation, because
there is no safepoint inside the sequence for a root to matter at.

## What a targeted probe rules out (2026-09-02)

`probes/BoxUnboxReloc.java` -- a hot compiled unbox of long-lived boxed
receivers, interleaved with garbage so their pages fragment and become
compaction candidates. **8 runs per arm, intrinsic ON and OFF, zero crashes and
zero wrong answers**, with all three ingredients measured as ENGAGED in the same
run:

| ingredient | how it was confirmed |
|---|---|
| the intrinsic | 3 sites claimed (`CRATONVM_DBG_ATOMIC_INTRINSIC=1`) |
| relocation | `objects_relocated=34629`, `compaction_cycles=2` (`CRATONVM_GC_STATS=1`) |
| the bail edge | `nullBails=19200` -- null receivers deopt through reason 6 |

The bail edge is in there deliberately: it is the only CALL anywhere near this
sequence, it is taken with the receiver already popped from the simulated
operand stack, and it was the one remaining place a safepoint could open a
window. It does not.

**Read those counters before believing any result from this probe.** The first
version of it ran clean 3/3 and meant nothing: it allocated the receivers in
one dense block, so `objects_relocated` was 0 and the collector never had a
reason to move them. A relocation defect cannot be exercised by a run that
relocates nothing.

`probes/BoxUnboxRelocMT.java` varies the one thing left -- thread count. Four
mutator threads unboxing the same tables while a fifth fragments them, 30 s per
run, **3 runs per arm, no crash and no wrong answer in any of them**:

| arm | compaction cycles | objects relocated | null bails |
|---|---|---|---|
| intrinsic ON | 173 / 204 / 211 | 3.2M / 3.7M / 3.9M | ~300k |
| intrinsic OFF | 150 / 226 / 281 | 2.7M / 4.1M / 5.0M | ~480k |

Five million relocations with the family enabled, and nothing. So whatever H2
does, it is not simply "unbox a relocating receiver on several threads".

Two extraction traps this cost, both worth avoiding on the next attempt.
`objects_relocated=` appears on more than one line, and grepping the whole log
for the LAST one reports 0 while the summary line says 3.2M -- restrict to
`zgc-features:` first. And at 6 threads the workload stops compacting
altogether (`objects_relocated=0` in every run), so a thread count chosen for
"more pressure" can quietly remove the very ingredient being tested.

The lead that remains is what `TestRandomMapOps` does that this does not:
receivers that are not `Long`/`Integer` from `valueOf`, a deopt from somewhere
other than the null check, or an interaction with the map's own structure.

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
