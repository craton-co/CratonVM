# `TestOutOfMemory` crashes the whole process (`SIGABRT`) instead of a catchable `java.lang.OutOfMemoryError`

## Status
**OPEN** — likely the already-known, explicitly-not-fully-eliminated
fallback path from a previously FIXED bug, not investigated further this
session. Found while re-running the H2 suite's HANG classes with a longer
(1500s) per-class timeout — this class doesn't hang, it crashes the whole
VM process (`timeout: the monitored command dumped core`).

## Severity
**MEDIUM** — narrow trigger (a workload that genuinely exhausts both young
*and* old generation), but the failure mode is a full process abort rather
than a catchable exception, which is a much worse outcome than a normal
test failure for anything that depends on `OutOfMemoryError` being
recoverable.

## Affected test class
`org.h2.test.db.TestOutOfMemory` — by design, this test deliberately drives
the JVM to run out of memory and checks that H2 handles it gracefully (a
catchable `OutOfMemoryError`, not a crash).

## Symptom
```
FATAL: OutOfMemoryError: young gen exhausted — tried to allocate 53087104 bytes,
  from-space has 485786952/536870912 used
#
# A fatal error has been detected by the CratonVM Runtime Environment:
#  SIGABRT at pc=0x785b3129ec0c, addr=0x3e80019c5be, pid=1689022, ...
```
Preceded by several minutes of:
```
STW cross-thread JIT takeover is still waiting for cooperative mutators rounds=64 pending=1 taken=0
```

## Likely root cause (not independently re-confirmed this session)
`docs/internal/fixed-suite-bugs/tomcat/02-native-young-gen-oom-abort-FIXED.md`
already documents and fixes this exact symptom family for the common case:
native allocators that hit a full young generation used to
`std::process::abort()` immediately; the fix spills the allocation into the
**old** generation instead (`try_alloc_object_old`), since old-gen
allocation relocates nothing and is therefore safe to do from a native
context holding un-rooted `ObjectRef`s. Critically, that doc's own fix
description ends with: **"Falls through to the original abort only if old
gen is also full."** `TestOutOfMemory` is, by its very design, a test that
tries to exhaust *all* available memory — it is a highly plausible
candidate for actually hitting that still-intentionally-preserved fallback
abort path, rather than being a new, distinct defect.

## What would confirm/refute this
1. Check whether HotSpot JDK25 passes `TestOutOfMemory` cleanly at the same
   default heap size used by the H2 suite runner (`--Xmx 1g`) — if HotSpot
   also struggles (or the test is known to be heap-size-sensitive/flaky
   even there), this may not be a CratonVM-specific gap at all.
2. If HotSpot passes cleanly, determine whether CratonVM's total
   young+old capacity at this `-Xmx` is undersized relative to what the
   test needs to complete its OOM-and-recover cycle (matching the
   already-fixed doc's own diagnosis that CratonVM's young generation, at
   `Xmx/4`, is far smaller than HotSpot's equivalent working set
   assumption) — i.e. whether this is the SAME general
   young/old-generation-sizing mismatch, just needing the old-gen ceiling
   raised or made adaptive too, not a wholly new mechanism.
3. If old gen is confirmed full (not just young), decide whether a
   catchable `OutOfMemoryError` can be thrown from the panicking native
   allocators in that case (matching real JVM behavior, which generally
   still manages to throw a catchable OOM even under severe memory
   pressure, reserving emergency headroom for exactly this) instead of
   aborting — this would be the actual fix, if warranted.

None of the above was done this session — this doc exists to flag the
crash and point directly at the most likely existing explanation, so
whoever picks it up doesn't have to re-derive the connection to the
already-fixed Tomcat doc from scratch.

## Repro
```bash
cd apps/h2database/h2
<cratonvm-bin> --java-home /home/victor/jdk25 --nojit --Xmx 1g \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.db.TestOutOfMemory
```
Reproduced once this session (~233s to crash); not yet confirmed
deterministic across repeated runs.
