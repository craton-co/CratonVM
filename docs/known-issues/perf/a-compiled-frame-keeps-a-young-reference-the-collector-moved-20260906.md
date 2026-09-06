# A compiled frame keeps a young reference the moving collector moved — and the give-back turns it into a 10-second SIGSEGV

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
