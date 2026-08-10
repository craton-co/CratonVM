# G1 GC: deterministic SIGSEGV from an unregistered JIT frame — same root cause as the default-GC slowdown, but unsafe under G1

| | |
|---|---|
| **Status** | OPEN — high severity, real crash |
| **Discovered** | 2026-08-10, complete 651-class Tomcat suite run under `-XX:+UseG1GC`, 2 shards |
| **Related** | [gc-moving-young-persistent-nonmoving-fallback-regression.md](gc-moving-young-persistent-nonmoving-fallback-regression.md) — same GC/JIT root-coverage condition, different (safe) consequence under the default generational collector |

## Symptom

4 of 651 classes crash the whole process with a native SIGSEGV under G1,
where the identical classes pass (or at worst hang/slow down) under the
default generational GC:

- `org.apache.catalina.startup.TestHostConfigAutomaticDeploymentModification`
- `org.apache.catalina.startup.TestHostConfigAutomaticDeploymentWar`
- `org.apache.catalina.startup.TestHostConfigAutomaticDeploymentWarXml`
- `jakarta.servlet.http.TestHttpServletDoHeadInvalidWrite1ValidWrite511`

**Three of the four crash at the exact same faulting instruction**
(`pc=0x00007FF74B49243F`, RVA `0x135243F`) — a single JIT-compiled code
address, not three independent defects:

```
# A fatal error has been detected by the CratonVM Runtime Environment:
#  EXCEPTION_ACCESS_VIOLATION (SIGSEGV) (0xC0000005) at pc=0x00007FF74B49243F
#  Faulting access: read at address 0x00007FFB3B8BCD38
#  gc collector: g1
#  gc young-gen policy: moving (Cheney young copy)
#  gc young-gen last incomplete-coverage reason: innermost-rbp-belongs-to-unguarded-callee
```

The fourth (`TestHttpServletDoHeadInvalidWrite1ValidWrite511`) crashes at a
different address but with the identical diagnostic shape, plus one more
explicit line that names the mechanism directly:

```
#  gc young-gen last incomplete-coverage reason: innermost-rbp-belongs-to-unguarded-callee
#  gc young-gen: the faulting thread had an UNREGISTERED JIT frame on its
#  native stack in the last root-gathering pass (no precise root map for it)
```

## This is the same condition as the default-GC slowdown, with a different (unsafe) outcome

`innermost-rbp-belongs-to-unguarded-callee` is the exact reason string
documented in
[gc-moving-young-persistent-nonmoving-fallback-regression.md](gc-moving-young-persistent-nonmoving-fallback-regression.md)
as the trigger for the default generational GC's persistent fallback to a
non-moving sweep. Under the default GC, hitting this condition means the
collector plays safe (skip compaction for that cycle) at a steep throughput
cost. **Under G1, the equivalent situation instead reads through a stale or
never-rooted pointer and crashes** — an unguarded/unregistered JIT frame's
"root" turned out to point at freed or relocated memory rather than being
conservatively retained.

In other words: the default GC's "slow but safe" fallback and G1's crash are
two different failure responses to the *same underlying gap* — JIT frames
that G1 (and, under the generational GC, the moving-young collector) cannot
prove a complete rewritable root map for. The generational GC's fallback path
happens to mask the gap by degrading to non-moving; G1 has no equivalent
guard and dereferences whatever garbage is sitting where the object used to
be.

## Reproduction

```powershell
cd C:\craton\CratonVM\apps\tomcat
<cratonvm.exe> --java-home "<real JDK 25>" -Xmx2g -XX:+UseG1GC -cp (Get-Content .suite\cp.txt) `
  org.junit.runner.JUnitCore org.apache.catalina.startup.TestHostConfigAutomaticDeploymentWar
```

Not yet checked whether this reproduces standalone (single class, no shard
contention) or only appears under the full-suite run's allocation pressure —
all 4 occurrences here came from a 651-class, 2-shard run.

## Suggested next step

Since G1 crashes exactly where the generational GC's own diagnostic already
names the unsafe condition (`innermost-rbp-belongs-to-unguarded-callee`), the
fix likely belongs in whatever code registers/guards JIT frames for root
scanning — G1's root-scanning path needs the same conservative treatment the
generational collector's fallback already applies, or (better) the actual
"unguarded callee" gap in frame registration should be closed for both
collectors rather than papered over per-backend.
