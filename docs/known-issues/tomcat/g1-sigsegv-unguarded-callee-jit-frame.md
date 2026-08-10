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

### 2026-08-10: it does NOT reproduce standalone — and that blocks the discriminator

Run exactly as prescribed above (`-Xmx2g -XX:+UseG1GC`, one class per process,
`JUnitCore`, quiet box), each class in two arms — plain G1, and G1 under
`CRATONVM_G1_COVERAGE_PIN=1`. Binary `cratonvm-g1fix-20260810.exe`
(`dev` + the diagnostics from the Spring Boot G1 page), 600 s cap:

| Class | G1 baseline | G1 + `COVERAGE_PIN` |
|---|---|---|
| `…AutomaticDeploymentModification` | TIMEOUT 600 s | TIMEOUT 600 s |
| `…AutomaticDeploymentWar` | **EXIT=0 (passed), 287 s** | TIMEOUT 600 s |
| `…AutomaticDeploymentWarXml` | EXIT=1 (test failure), 152 s | EXIT=1, 532 s |
| `TestHttpServletDoHeadInvalidWrite1ValidWrite511` | **EXIT=0 (passed), 85 s** | EXIT=1, 575 s |

**Zero `EXCEPTION_ACCESS_VIOLATION` in any of the eight runs.**
`…DeploymentWar` — one of the three that crashed at the *identical* faulting
instruction in the suite run — passes cleanly standalone in 287 s, and
`TestHttpServletDoHead…` passes in 85 s. So the crash needs the full-suite
conditions (allocation pressure, 2-shard concurrency, or cross-class state);
it is not a property of these classes in isolation.

**Consequence: `CRATONVM_G1_COVERAGE_PIN` cannot be used this way.** The lever
only tells you something when a crash is there to survive it, and standalone
there is no crash. Running it under the full suite is possible in principle but
expensive and probably impractical: the lever makes G1 refuse to evacuate, and
its cost is visible even here — `…DeploymentWar` goes from a 287 s pass to a
600 s timeout, and `TestHttpServletDoHead…` from an 85 s pass to a 575 s
failure. Those pin-arm timeouts are the lever's documented no-op-pause cost,
**not** evidence about the defect, and must not be read as either a pass or a
fix. (`…DeploymentModification` times out on *both* arms, so it carries no
signal at all here; and `…DeploymentWarXml`'s EXIT=1 is an ordinary test
failure on both arms, not a crash.)

Next attempt should therefore reproduce under suite-like conditions — several
of these classes concurrently, or the shard that contained them — before
reaching for any lever.

## Very likely the same defect as the Spring Boot G1 corruption — and the mechanism above may be the wrong one

See [`../springboot/g1-fullsuite-regression-20260808.md`](../springboot/g1-fullsuite-regression-20260808.md)
§3/§3b. That page characterizes a G1-only corruption on a different suite whose
evidence lines up with the crashes here:

* **G1-only, where the default collector is merely slow.** Same asymmetry,
  measured repeatedly (`CloudFoundryActuatorAutoConfigurationTests`: default
  TIMEOUT 4/4 at 900 s vs G1 PASS 3/3).
* **A matching `EXCEPTION_ACCESS_VIOLATION`** under G1
  (`CacheAutoConfigurationTests`, 272 s), whose dump has ASCII class-name bytes
  in shadow-stack slots — reads landing in memory that was reset and reused.
* **JIT-dependence, established by control.** `--nojit` under G1 is completely
  clean on the loudest reproducer (`Log4J2LoggingSystemTests`: PASS 61/61, zero
  corruption reports) where JIT-on fails 18/61 with 563 zeroed-header reads.
  That matches this page's "unregistered JIT frame" correlation.

**But the mechanism this page names is not what that investigation found, and
it is worth not fixing the wrong thing.** This page assumes an unguarded JIT
frame's *root* points at freed or relocated memory. The measured chain there is
different: G1 walks a **JIT-pinned Eden region** linearly as a remembered-set
source (pinned regions are held OUT of the collection set, so they get walked
rather than evacuated), the walk hits an unwalkable hole and is **abandoned**,
and every **heap reference past that offset is therefore never rewritten** by
the pause. The frame's roots are enumerated fine; it is the heap slots that go
stale. Both stories end in "dereference a stale pointer", so the crash dumps
cannot separate them — but they have different fixes.

Two things from that page apply directly here:

* `CRATONVM_G1_COVERAGE_PIN=1` is the discriminator. Under it G1 refuses to
  evacuate on any pause whose root coverage is incomplete, so it moves nothing.
  **A crash that survives that lever is not caused by a relocation the root set
  failed to cover** — which would refute this page's premise outright. It is
  cheap and has never been run against these four classes.
* Seven hypotheses are already refuted there with controls — including the
  `metadata_pin_deferrable` G1 asymmetry, three JIT allocation gates, and an
  abandoned-TLAB-at-thread-death theory that had a hexdump behind it and still
  changed nothing — plus the whole TLAB-bookkeeping branch closed by
  enumeration. Worth reading before spending a cycle here.

## Suggested next step

Since G1 crashes exactly where the generational GC's own diagnostic already
names the unsafe condition (`innermost-rbp-belongs-to-unguarded-callee`), the
fix likely belongs in whatever code registers/guards JIT frames for root
scanning — G1's root-scanning path needs the same conservative treatment the
generational collector's fallback already applies, or (better) the actual
"unguarded callee" gap in frame registration should be closed for both
collectors rather than papered over per-backend.
