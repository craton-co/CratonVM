# A compiled frame keeps a young reference the collector moved — FIXED 2026-09-06

| | |
|---|---|
| **Status** | ✅ **FIXED.** A peer thread's compiled frames were discharged from the cross-thread coverage obligation by PINNING their conservative roots — on a collector that does not honour pins and structurally cannot. The generational young collector is Cheney copying: from-space is reclaimed wholesale, so a "pinned" object is faithfully kept ALIVE at a NEW address, with nothing having rewritten the peer's frame. The credit now requires the collector to honour the pin. |
| **Scope** | `-XX:+UseGenerationalGC` only, JIT only, release only, multi-threaded only. G1 and ZGC were never affected — both consume `gc_quiescence::pinned_jit_roots_snapshot()` and withhold the region/page. |
| **The fix** | `VmHeap::honours_conservative_pins()`, passed into `refresh_moving_young_coverage_for_collection` as a fourth condition on the pinned-peer credit. |

## The evidence, in one table

`-XX:+UseGenerationalGC --Xmx 512m` over `org.h2.test.jdbc.TestPreparedStatement`,
`CRATONVM_GEN_UNCOMMIT=1` as the detector on every arm:

| arm | crashes |
|---|---|
| before the fix | **4/5** |
| after the fix | **0/5** |
| after the fix, `CRATONVM_XT_PINNED_PEER_UNPINNABLE=1` | **5/5** |

The third row is the point. A 10-second stochastic reproducer going quiet is not
a fix; the same binary with the old accounting restored crashes 5 times out of 5,
faster than the original, so the first row's absence is attributable.

## How it was found

Two bisects over levers, three reps each, `CRATONVM_GEN_UNCOMMIT=1` throughout.

| lever | crashes |
|---|---|
| control | 3/3 |
| `CRATONVM_NO_MOVING_YOUNG=1` | 0/3 |
| **`CRATONVM_XT_JIT_COVERAGE_HANDSHAKE=0`** | **0/3** |
| `CRATONVM_JIT_SPILL_NARROW=0` | 2/3 |
| `CRATONVM_JIT_CALL_SPILL_ELISION=0` | 3/3 |
| `CRATONVM_JIT_SPILL_ARGS_PUBLISHED=0` | 3/3 |
| `CRATONVM_JIT_SAFEPOINT_REG_SPILL=all` | 2/3 |

The spill levers are the ones the first write-up nominated — the blind GPR
safepoint spill, whose slots `FrameLayout::is_register_image` excludes from
verification. **None of them is the cause.** The handshake is, and the second
bisect split it in two:

| lever | crashes |
|---|---|
| `CRATONVM_XT_PINNED_PEER_DEPTH=0` (drop the pinned credit) | 0/3 |
| `CRATONVM_XT_PINNED_PEER_PUBLISH_ONLY=1` (publish, don't credit) | 0/3 |

and `CRATONVM_DBG_XT_COVERAGE=1` printed the accounting that names it. The last
line before the fault:

```text
[xt-coverage] peer_depth=2 proven=0 pinned=2 accounted=true
```

`proven=0` — nobody proved anything. `pinned=2` — the pin credit alone
discharged the whole peer depth, and the cycle relocated. The one run in that
arm that did NOT crash had:

```text
[xt-coverage] peer_depth=2 proven=0 pinned=0 accounted=false
```

## Why the pin was not a discharge

`refresh_moving_young_coverage_for_collection` accepts a moving cycle when
`proven + pinned >= peer_depth`. The `pinned` term already carried three
conditions and its own paragraph explaining that a single unpinned window voids
the whole credit. What it never asked was whether **the collector honours a pin
at all**.

* **G1** withholds the pinned regions from the collection set (`g1.rs` consumes
  `pinned_jit_roots_snapshot()`).
* **ZGC** does the same at page granularity.
* **Generational** has **zero readers** of that snapshot in `gen_heap.rs` or
  `gen_evac.rs`, and cannot acquire one: a Cheney young collection reclaims
  from-space wholesale, so every live object in it moves by construction. The
  pinned addresses reach that backend only through the ROOT set — which keeps
  them alive, and says nothing about keeping them put.

So on the generational collector the credit discharged a peer's obligation
against a pin nobody applied. That distinction — *alive* is not *put* — is the
whole defect.

## After the fix, the accounting reads

```text
[xt-coverage] peer_depth=2 proven=0 pinned=0 pins_honoured=false accounted=false
[xt-coverage] peer_depth=3 proven=3 pinned=0 pins_honoured=false accounted=true
[xt-coverage] peer_depth=6 proven=4 pinned=0 pins_honoured=false accounted=false
```

Proof-based acceptance is untouched (row 2); only the pin-based discharge is
refused, and only where the pin is not honoured. G1 and ZGC keep the credit that
was built for them — it is what stopped ZGC refusing to compact on the H2
fragmentation family, 219 of 227 refusals.

## Two things that are NOT the fix, and were believed to be

* **The blind GPR safepoint spill.** The first write-up nominated it because
  `audit_stale_frame_words` classes 11 of its witnesses `gpr-safepoint-spill`
  and marks that class "unmodelled". Four independent levers over that spill
  leave the crash in place. The witnesses are real and are an upper bound — the
  audit walks the whole frame band, so a dead spill slot holding a moved
  object's old address is counted and is harmless.
* **The peer resume path not remapping compiled frames.**
  `apply_pointer_map_to_thread` does call `remap_active_jit_frames`,
  `remap_register_image_words` and the shadow-stack remap. That path is
  complete; it was simply never reached for objects that moved out from under a
  peer whose coverage was discharged by a pin.

## Kept, because the reproducer is worth more than the bug

`CRATONVM_XT_PINNED_PEER_UNPINNABLE=1` restores the old accounting on the same
binary, which is what makes this a fix rather than a quiet reproducer.

`CRATONVM_GEN_UNCOMMIT` is the detector that made a silent stale read into an
attributable SIGSEGV, and it **went back to ON by default on 2026-09-06** once
this closed — it had been reverted for a day precisely because of this defect.
So the default path now carries the loudness: any stale young reference that
survives anywhere in the VM faults immediately, with the released span, the site
and the code buffer printed, instead of reading the previous cycle's bytes.
`CRATONVM_GEN_UNCOMMIT=0` is the first thing to reach for if a compiled frame
faults on a young address.

---

## The original report, as filed on 2026-09-06

Kept below for the reasoning and the raw signatures. Read the sections above
first: this half nominates the blind GPR spill, and the bisect refuted it.

| | |
|---|---|
| **Status** | OPEN. The stale reference is real, reproducible in ten seconds, and pre-dates everything on this page. What changed on 2026-09-05 is that `CRATONVM_GEN_UNCOMMIT` was defaulted ON, which converts the stale READ into a SIGSEGV. That default is reverted (see below); the reference is still stale with it off, it is just silent again. |
| **Scope** | `-XX:+UseGenerationalGC` only. The default collector is unaffected. `--nojit` is unaffected. Independent of heap size. |
| **Reproducer** | 10 s, one test class. |

## The reproducer

```bash
CMD=$(sed 's/^DODCMD //' /data/dod-out/cmd-h2jdbc-compat.txt)
CMD=${CMD/--Xmx 2g/-XX:+UseGenerationalGC --Xmx 512m}
CMD=$(echo "$CMD" | sed 's#DodH2JdbcSuite .*#DodH2JdbcSuite org.h2.test.jdbc.TestPreparedStatement#')
CRATONVM_GEN_UNCOMMIT=1 $CMD
```

```text
#  SIGSEGV at pc=0x7168f7bc81ab, addr=0x716944f75dd7
#  rax=0x716944f75dc8  rsi=0x716944f75dc8
#  fault addr is inside a RECENTLY DECOMMITTED heap span:
#      base=0x716944000000 len=0x2200000 site=unbumped-middle
#  fault pc is inside a LIVE registered code buffer: base=0x7168f7bc8000 cap=0x2280
```

`addr − rax = 15`, which is `GC_FLAGS_BYTE_OFFSET`. So the faulting instruction
is the compact `getfield` fast path's flags load through a receiver that points
into the semi-space the previous collection evacuated and then handed back to
the OS.

## Attribution — one binary, five arms

| arm | rc | wall |
|---|---|---|
| default (`CRATONVM_GEN_UNCOMMIT` ON, as of 2026-09-05) | **139** | 10 s |
| `CRATONVM_GEN_UNCOMMIT=0` | 0 | — |
| `CRATONVM_GC_RESERVE=0` (nothing decommits) | 0 | — |
| `--nojit` | 0 | — |
| `CRATONVM_GC_OBJECT_STARTS=0` (the OTHER default flipped that day) | 139 | — |

The third and fourth rows are the ones that name the defect rather than the
switch. `CRATONVM_GC_RESERVE=0` keeps every granule mapped, so the same stale
reference reads stale bytes instead of faulting — the fault is the *reporting*,
not the *bug*. `--nojit` removes it entirely, so the holder is compiled code.

## RELEASE ONLY, and re-confirmed after a dev merge

**The debug binary does not reproduce it.** Four arms of the recipe above on a
debug build — including `CRATONVM_GEN_UNCOMMIT=1` — all `rc=0`. Read that as a
statement about codegen, not about the defect: the stale slot is a compiled
frame's, and the debug tier inlines and spills differently. Every reproduction
below is a release binary. An arm that "passes" on debug has not tested this.

Re-run on 2026-09-06 against a release build of the branch merged with a dev
that had gained several root and skip-span fixes in between — among them *"the
ninth exit: a stack-trace pause published skip spans and never retired them"*,
which was the most plausible candidate for having closed this by accident:

| arm | reps | rc |
|---|---|---|
| `CRATONVM_GEN_UNCOMMIT=1` | 3 | **139, 139, 139** (6 s, 13 s, 7 s) |
| default (off) | 3 | 0, 0, 0 |

So none of those fixed it, and the reproducer is stable across a week of dev
movement.

## Who holds it

`CRATONVM_DBG_STALE_FRAME_WORDS=1`, which scans each compiled frame AFTER
`remap_one_jit_frame` has rewritten every slot the oop maps name:

```text
[STALE-FRAME-WORD] method=org/h2/command/Command.stop:(Z)V slot=[rbp-0x250]
    class=gpr-safepoint-spill stale=0x71694df5e438 should_be=0x716944013dd0
    frame_size=896 maps=13 covered=true shadow_covered=true
[STALE-FRAME-WORD] method=org/h2/mvstore/tx/Transaction.commit:()V slot=[rbp-0x1e0]
    class=gpr-safepoint-spill stale=0x71694df5cc78 should_be=0x716944013bd0
    frame_size=864 maps=13 covered=false shadow_covered=false
```

Two methods, 40 witnesses in the first 10 s, by slot class:

| class | count |
|---|---|
| `gpr-safepoint-spill` | 11 |
| `operand-spill` | 8 |
| `other` | 21 |

`gpr-safepoint-spill` is one of the four classes `audit_stale_frame_words`
itself marks **unmodelled** — the predicted defect. It is the blind
all-registers spill the per-call-spill work measured at fourteen stores: the
emitter does not know which of those registers hold oops, so the slots it writes
them into are in no oop map, and nothing rewrites them.

**Read the count as an upper bound, not a defect count.** The audit walks every
word of the frame band, so a dead spill slot that still contains a moved
object's old address is counted and is harmless. What is NOT an upper bound is
the SIGSEGV: `rax` was loaded from a frame slot and dereferenced.

`Transaction.commit` reporting `covered=false shadow_covered=false` while a
moving collection ran beside it is the second thread to pull: the coverage gate
(`moving_young_unpublished_frame_oop_present`) is walked over
`JIT_ENTRY_CHAIN`, which is THREAD-LOCAL, and H2 is not single-threaded.

## Why the default went back to opt-in rather than the fault being fixed here

`uncommit_evacuated_young`'s own doc predicted this exact fault, named the
opt-out as the first thing to reach for, and shipped ON anyway on the strength
of a 90/0 HotSpot-differential regression suite. That suite does not run this
corpus. And the same measurement that justified the flip put the benefit at
**2552 ms against 2562 ms** — free to within noise. A change with no measurable
benefit does not get to crash a supported collector on a real workload by
default.

The switch stays, and it is now the sharpest instrument in the tree for this
family: it turns a stale young reference from a silent read of the previous
cycle's bytes into an immediate, attributable SIGSEGV with the span, the site
and the code buffer already printed.

## Where the rest of this lives

`Flags::gen_uncommit` carries the five-arm attribution and the reason the
default went back to opt-in. The retired cross-collector common-work write-up is
where the give-back was built and where its fault window was predicted in
writing; the per-call blind GPR spill record has the emitter that writes the
`gpr-safepoint-spill` slots.

## What a fix has to do

1. Put the blind GPR safepoint spill's slots into the oop map, or stop spilling
   oop-bearing registers blindly. The per-call-spill record has the emitter.
2. Make the moving-young coverage gate see PEER threads' compiled frames, not
   only the collecting thread's chain.

Either one is checkable against this reproducer in ten seconds, with
`CRATONVM_GEN_UNCOMMIT=1` as the oracle.
