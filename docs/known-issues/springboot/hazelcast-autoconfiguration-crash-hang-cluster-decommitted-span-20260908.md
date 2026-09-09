# Hazelcast autoconfiguration tests crash or hang on every GC — a `java/nio/Bits$1` "total silent data loss" read precedes every crash/hang instance that has one

| | |
|---|---|
| **Status** | OPEN, filed 2026-09-08. Found on the complete Spring Boot suite run (all 1991 classes, 3 GC arms) at dev tip `a58ebd5c`. Not root-caused. |
| **Scope** | `module/spring-boot-hazelcast`, JIT on, real JDK 25, `-Xmx2g`, `--XX:UseGc {Generational,G1,Z}`. HotSpot passes both classes cleanly on every arm (0 failures). |
| **Reproducer** | `org.springframework.boot.hazelcast.autoconfigure.HazelcastAutoConfigurationClientTests` (12 tests) and `HazelcastAutoConfigurationServerTests` (20 tests) |

## The result matrix

| Class | Generational | G1 | ZGC |
|---|---|---|---|
| `HazelcastAutoConfigurationClientTests` | **CRASH** (SIGSEGV, rc=139) | HANG (300.0s) | PASS (12/12) |
| `HazelcastAutoConfigurationServerTests` | **CRASH** (SIGSEGV, rc=139) | HANG (308.4s) | HANG (300.0s) |

Both classes fail on every collector they don't outright crash on. Neither
class has a single clean pass on Generational or G1.

## The crash

Both Generational crashes fault inside the same call shape — a native call
invoked through `safe_native_call_impl::{closure_env#1}` — and the VM's own
crash handler reads the fault as a **decommitted heap span being touched**:

```
# A fatal error has been detected by the CratonVM Runtime Environment:
#  SIGSEGV at pc=0x5b9a449f5f3c, addr=0x749bf6537150, pid=2374151
#  fault pc is in NO recently freed code buffer
#  fault addr is inside a RECENTLY DECOMMITTED heap span: base=0x749bf2800000 len=0x4200000 site=unbumped-middle
#    *** and NOT re-committed since. Something TOUCHED a span the collector proved dead: either a stale
#        pointer read it, or a writer wrote into it. ***
#  fault pc is in NO live registered code buffer
```

The handler's own two hypotheses (paraphrased from its output): a compaction
slide wrote into free space without going through
`Arena::commit_for_relocation`, or a reader held a stale pointer — a missing
root, or a sweep that mis-sized the live set (`site=unbumped-middle` names a
cursor that passed over still-live bytes). It explicitly declines to pick
between them from the faulting address alone.

## The lead: a same-run residual read the VM already calls out as unsafe

Seconds before each crash (and before the G1 hangs), the same warning fires
against the same receiver shape:

```
WARN cratonvm::gc::guard: gen_heap::get_field: out-of-bounds field read dropped
  (caller used slot index past receiver's layout — the class layout is correct,
  the caller's slot computation is not). ... If class_name is a REAL JDK class
  and num_slots equals real_field_count, it is not [benign]: a native is
  reading a synthetic layout that this class does not have, and the writer of
  that layout is a different native writing a different one. That is total,
  silent data loss.
[RESID-DIAG READ] class=java/nio/Bits$1 index=0 num_slots=0 obj=0x749bf4d29b38 real_field_count=Some(0)
   0: get_field                  gc/src/gen_heap.rs:5223
   1: get_field                  vm/src/vm/vm_exec.rs:12680
   2: buffer_pool_get_name       native-builtins/src/shared_secrets_bridge.rs:3070
   3: safe_native_call_impl      vm/src/vm/vm_exec.rs:4081
```

`java/nio/Bits$1` is a real JDK class, and `num_slots` (0) equals
`real_field_count` (`Some(0)`) — which is exactly the condition the warning's
own text calls **not** benign ("total, silent data loss"), as opposed to the
benign case (a speculative probe against a fabricated/collection class that
doesn't match).

The warning's presence correlates with every crash/hang that has one:

| Class | Arm | `RESID-DIAG READ` hits | Outcome |
|---|---|---:|---|
| Client | Generational | 1 | CRASH |
| Client | G1 | 1 | HANG |
| Client | ZGC | 0 | PASS |
| Server | Generational | 2 | CRASH |
| Server | G1 | 1 | HANG |
| Server | ZGC | 0 | HANG |

Every instance with the warning fails. The one exception in the other
direction — ZGC's `ServerTests` hang with **zero** warning hits — means the
warning is not the whole story: either a second, independent hang cause exists
on that class, or the read is timing-sensitive enough that it simply didn't
fire before the hang did on that run. Per this repo's own standing lesson, a
recorded residual's reason is a hypothesis, not a verdict — this correlation
is a lead to chase, not a closed causal chain, and `buffer_pool_get_name`
reading a wrong layout on `java/nio/Bits$1` has not been shown to be the same
event as the later decommitted-span fault, only adjacent to it in the same
process shortly before.

## What is NOT yet known

* Whether the residual read and the crash share a root cause, or are two
  independent defects that happen to co-occur on this workload.
* Why ZGC's `ClientTests` neither warns nor fails, while `ServerTests` hangs
  with no warning at all — same module, same JIT/heap settings, different
  outcome.
* Whether `site=unbumped-middle` here is the same specific defect as any of
  the already-fixed decommit-fault bugs in this repo (e.g.
  `bug-testlargeblob-segv-decommit-under-live-memcpy-20260904.md`,
  `generational-young-sweep-frees-an-interpreter-held-object-FIXED-20260908.md`)
  or a new instance of the general class those fixed.

## Repro

```bash
cd apps/spring-boot-suite-runner
pwsh -c "./run-spring-boot-suite.ps1 -Category all -ClassList <TSV with just these 2 rows> \
    -Vm craton -Jit on -Exe <cratonvm> -JdkHome <jdk25> \
    -CratonArgs @('--XX:UseGc','Generational') -TimeoutSec 300"
```
Both classes are single-process, single-JVM-launch runs (no `--only`
sub-selection needed beyond the class list) — `HazelcastAutoConfigurationClientTests`
crashes in well under a minute; `ServerTests` takes longer but reproduces the
same way.

## Next

1. Reproduce in isolation (this run was on a heavily loaded shared host —
   confirm the crash/hang reproduces standalone before trusting the exact
   timings, though a SIGSEGV itself cannot be a load artifact).
2. Pin down whether the `Bits$1` residual read and the fault touch the same
   object/span, e.g. by logging the freed-span address range from the
   `RESID-DIAG` event's own `obj=` field against the crash report's
   `fault addr`/`base`/`len`.
3. Find what in the Hazelcast client/server autoconfiguration path drives
   `buffer_pool_get_name` — `com.hazelcast.shaded.org.jctools` and
   `sun.misc.Unsafe::putOrderedLong` both appear in this class's log, so the
   suspect surface is jctools' off-heap/Unsafe buffer pooling calling into
   `SharedSecrets`-style native shims.
4. Explain the ZGC asymmetry (Client clean, Server hangs with no warning) —
   it may separate "the residual-read lead" from "a second, ZGC-specific hang"
   rather than being one defect.
