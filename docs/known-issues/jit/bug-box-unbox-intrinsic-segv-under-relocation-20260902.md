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

## The repro is currently BLOCKED by an earlier failure (2026-09-02)

Run on `dev@08a1711e5`, `livedbg`, quiet host, against H2 built at
`apps/h2database/h2`. **It cannot reach the window this page measures in.**

`TestRandomMapOps` dies of `seed:0 op:1033 java.lang.AssertionError: (1810,
null)` after 11-22 s, having completed ZERO passes -- where HotSpot 25 on the
same classpath completes at least nine. The SIGSEGV this page records appears at
25-183 s. The run is over before that window opens.

It is not this intrinsic. Five runs per arm at `--Xmx 256m`, three per arm at
384m / 512m / 1g:

| arm | outcome |
|---|---|
| `CRATONVM_JIT=box-unbox-intrinsic` (family ON) | AssertionError, 0 SIGSEGV |
| shipped default (family OFF) | AssertionError, 0 SIGSEGV |
| `CRATONVM_ZGC_RELOCATE=0` | AssertionError, unchanged |
| `CRATONVM_JIT_NO_INLINE_FRAME_MAP=1` | AssertionError, unchanged |

Same op, same values, every arm. And it is **deterministic**: `seed:0 op:1033
(1810, null)` byte-identical across four consecutive runs.

That contradicts two things this page and its sibling rest on. This page says
the failures pre-dating the bisect range are a `NullPointerException` and a
fragmentation `OutOfMemoryError`; the blocker is neither, so a bisect scored the
way this page describes would now score every commit BAD for the wrong reason.
And `h2/bug-h2-testrandommapops-small-heap-corruption-20260829.md` says `--Xmx
1g` and `4g` are "clean over 1500 s each" -- at 1g this fails in 13-15 s. That
page also calls its defect one with "no reproducer worth bisecting yet". It has
one now, and it is 13 seconds long.

**Whoever takes this page next has to clear that first**, or bisect the SIGSEGV
on a tree where `op:1033` does not fire.

Not established, and measured to be unavailable rather than assumed away:
**whether the blocker is JIT-dependent.** A `--nojit` arm ran the full 1500 s
with no AssertionError -- and completed ZERO passes, where HotSpot completes one
about every 15 s. `TestRandomMapOps` prints an `op:` line only when it FAILS, so
a run that has not failed offers no evidence it ever reached op 1033. "1500 s
clean under `--nojit`" is therefore not a result; it is a run that may simply
be slower than the defect is deep. Scoring it as an arm would be the same
mistake as the `objects_relocated=0` probe above.

Making that arm answerable needs a progress signal the test does not currently
emit -- a per-op counter, or a seeded run bounded to a few thousand ops.

## Reproducing

```
cargo build --profile livedbg -p cratonvm-cli

# The classpath file omits H2's own output dirs; both are needed.
H=apps/h2database/h2
CP="$H/target/test-classes:$H/target/classes:$(cat $H/craton-testcp.txt)"

# Run from a scratch cwd: the test writes its database files beside you.
CRATONVM_JIT=box-unbox-intrinsic cratonvm --java-home $JDK25 --Xmx 256m \
  -cp "$CP" org.h2.test.store.TestRandomMapOps
```

Under three minutes on a quiet host. A CONTENDED host hides it -- the same
lever this repo has been bitten by before.

`CRATONVM_JIT_BOX_UNBOX_INTRINSIC=1` still works but now warns; the supported
spelling is the token above.

**As of 2026-09-02 this does not reach the SIGSEGV** -- see "The repro is
currently BLOCKED by an earlier failure".
