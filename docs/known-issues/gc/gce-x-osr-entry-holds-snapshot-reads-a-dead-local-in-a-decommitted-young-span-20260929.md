# The OSR entry's held-monitor snapshot reads a dead local whose young span was decommitted (SIGSEGV)

> **STATUS (2026-09-29, gce orchestrator): OPEN -- filed at the round close, not fixed.**
> Pre-existing: the base binary of the round (`adb9178bc`) crashes the same way.

## Symptom

`MtChurnProbe 4 60 48`, Generational, `-Xmx256m`, default flags, Linux release build:
intermittent SIGSEGV (rc 139) about two seconds in. Rate over 20 runs each, host under load
(`/data/gce-out/mtc/`):

| Binary | crashes |
|---|---|
| `cratonvm-gce-base` (`adb9178bc`) | 2/20 |
| `cratonvm-gce-e1` | 0/20 |
| `cratonvm-gce-e2` | 1/20 |
| `cratonvm-gce-e2` + `CRATONVM_XT_TAKEOVER_SIGNAL_JIT_ONLY=1 CRATONVM_XT_FIRST_PASS_GRACE_US=200` | 2/20 |

With `CRATONVM_GEN_PINNED_YOUNG_COPY=1` it crashed 2/2 (ve2 rows `y_gen_pg_mtchurn_on_{1,2}`);
that is the same pc, so the "pinned-copy gate crash" on
`gengc-r4w6-pinstale6-pinned-copy-default-flip-gate` is this defect at a higher rate, not a
pinned-copy defect.

The crash report:

```
SIGSEGV ... DATA READ of a NOT-PRESENT page
bytes at pc: 8b 5e 04 89 d8 83 e0 03 83 f8 02 74 2c 83 f8 01
fault addr is inside a RECENTLY DECOMMITTED heap span: ... site=unbumped-middle
this thread last applied a relocation map at cycle=0x8 path=1 of relocating_cycles=0x8
```

Symbolized (`addr2line` on the e2 binary; vaddr = file offset + 0x1000):
`MonitorTable::holds` (`vm/src/threading/monitor.rs`, the `header.mark_word.load`) called
from `OsrEntryHolds::snapshot` (`vm/src/runtime/interpreter/deopt_resume.rs`), which
`jit_bridge.rs` takes at every OSR entry (interpreter round i1 wave 28, lane L2).

## Cause (by reading)

`OsrEntryHolds::snapshot` walks EVERY local of the interpreter frame whose tag is
`VTAG_OBJECT` and reads the object's mark word through `MonitorTable::holds`. The root scan
does not treat every such local as a root: step 1 of `collect_roots` applies the per-bci
liveness filter, so a local no later bytecode reads is not a root, its referent can die, and
the moving young cycle can free and decommit its span. The slot keeps the stale address and
its object tag. The next OSR entry of that frame dereferences it.

## What would fix it

Either of:
1. Apply the same per-bci liveness filter in `OsrEntryHolds::snapshot` (only locals live at
   the frame's bci can name a monitor the frame still owns: javac keeps the monitor
   temporary live until its `monitorexit`).
2. Ask the monitor table which objects the thread owns and match those against the locals by
   address, without dereferencing a local first.

Option 1 matches what the collector already believes; option 2 is independent of liveness.

## Retire when

`MtChurnProbe 4 60 48` (`-XX:+UseGenerationalGC -Xmx256m`) 0 crashes in 50 runs on the
default arm and 0 in 20 with `CRATONVM_GEN_PINNED_YOUNG_COPY=1`, plus a unit test that
snapshots a frame holding a dead object-tagged local pointing at unmapped memory.
