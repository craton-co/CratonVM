# bintrees18 (JIT-on) — non-moving young-sweep throughput wall → timeout

## Symptom
Micro-benchmark `BenchSuite bintrees18` (binary-trees, depth 18), JIT-on,
`--Xmx 8g`:
- CratonVM-CPU: **rc=124 TIMEOUT (>360 s)**, no checksum.
- CratonVM-GPU: **rc=124 TIMEOUT (>360 s)**, no checksum.
- HotSpot: OK 758 ms, checksum `68332206`.
- TornadoVM: OK 760 ms, checksum `68332206`.

Reproduce:
```
target/release/cratonvm.exe --java-home "C:/Program Files/Java/jdk-25" \
    --Xmx 8g -cp bench BenchSuite bintrees18
```

## History / note on the symptom
The earlier session table recorded this as *"new bug: Rust stack overflow in
main-vm"*. In THIS run it presents as a **wall-clock timeout**, not a stack
overflow — the process makes forward progress but cannot complete the depth-18
allocation/collection workload within 360 s. Confirm whether the stack overflow
is gone or merely hidden behind the timeout (raise `--stack-dump-on-timeout` /
attach the VEH backtrace; see `reference_crash_debug_tooling`).

## Root cause (from prior investigation — docs consolidated in commit c7efb0d)
This is the documented **non-moving young-sweep throughput wall**:
- Two earlier bugs already FIXED: needs_gc() keyed on live occupancy (6a027e0),
  and the from-space walk reorder/hole-skip (9ff6b09) — these removed the
  corruption & thrash.
- The remaining limit (3): the non-moving sweep cannot tenure the depth-18 live
  set out of young. Selective promotion (`CRATONVM_SELECTIVE_PROMOTE`,
  default-OFF, commit 19af3ec/7b87baa) is **fundamentally unsafe under
  conservative JIT roots** — an object live only via an uncaptured JIT
  register/stack slot is not pinned → evacuated → stale ref → wrong-size tree.
  Verdict: unfixable without precise JIT stack maps. Do NOT re-attempt
  safepoint-spill (see docs/jit-safepoint-revert.md).

## Why HotSpot/TornadoVM are fine
Moving/generational collectors with precise stack maps tenure the live set
trivially; depth-18 binary-trees is a ~750 ms workload for them.

## What an agent should try next
1. Confirm timeout vs stack-overflow (instrument; capture the Rust backtrace if
   it IS overflowing — that would be a NEW, separate, fixable bug).
2. If it is the GC throughput wall: the only safe path is precise JIT stack maps
   so a moving/Cheney collector can run under live JIT frames. Large effort.
3. Interim: measure how far it gets in 360 s (objects promoted, GC count) to
   decide whether a bounded-depth pass is worth it.

## Out of scope
Not related to the EC JIT fix. Will still fail after the EC-fix rebuild.
