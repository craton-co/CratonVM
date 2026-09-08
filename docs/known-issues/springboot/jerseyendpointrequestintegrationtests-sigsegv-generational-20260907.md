# `JerseyEndpointRequestIntegrationTests` SIGSEGV under Generational GC — indexed load through a stale/garbage base pointer

## Status

**OPEN, new finding.** One occurrence in a 1975-class Spring Boot full-suite
run under `-XX:+UseGenerationalGC` (dev tip `4f2ed2687`, local Windows box,
2026-09-07). Not yet reproduced in isolation or bisected; filed with full
crash evidence rather than delayed for a repro, since the crash banner is
unusually complete.

## The crash

```
org.springframework.boot.security.autoconfigure.actuate.web.servlet.JerseyEndpointRequestIntegrationTests
rc=-1073741819 (0xC0000005, EXCEPTION_ACCESS_VIOLATION / SIGSEGV)

#  EXCEPTION_ACCESS_VIOLATION (SIGSEGV) (0xC0000005) at pc=0x00007FF616CC9961
#  Faulting access: read at address 0x000001F6B5FF6780
#  gc collector: generational
#  gc young-gen policy: moving (Cheney young copy)
#  gc young-gen actual: 0 moving cycle(s), 2 cycle(s) diverted to the NON-MOVING sweep
#  gc young-gen last incomplete-coverage reason: none
Faulting address decodes as an indexed load: [rbx+r13*8]
  rbx=0x000001F6B5FF6778  r13=0x0000000000000001   →  rbx + 1*8 = 0x…780, exactly the faulting address
```

Two `[moving-young] fallback` warnings preceded the crash (`reason=xt-helper-window-conservative-scan`,
peaks #1 and #2), and two `non-moving sweep: unlisted all-zero span … skipped, not freed`
warnings in between. Full banner, registers, and native frame addresses are
in the class's own log:
`apps/spring-boot-suite-runner/.suite-gen3gc-20260906/results/gen3gc/all-jit/logs/module_spring-boot-security.org.springframework.boot.security.autoconfigure.actuate.web.servlet.JerseyEndpointRequestIn-a0d9d711811f.err.log`
(untracked, local run output).

## What this is NOT, checked before filing

- **Not the `[moving-young]` fallback spiral** (`docs/internal/retired/moving-young-fallback-four-springboot-classes-RETIRED-20260818.md`,
  plain-text path — that directory is stripped from public git history).
  That page's own triage rule is explicit: fallback peaks of **single digits
  are harmless**, only peaks in the thousands (its own examples: #2048,
  #16384, 1238 cycles) drove the OOM/timeout spiral it tracked, and that
  spiral is independently closed (two unrelated fixes already on `dev`).
  This crash's fallback peaked at **#2** — squarely in the "harmless" range
  by that page's own calibration, so the fallback context is very likely
  incidental, not causal.
- **Not the `native-builtins` loop-carried stale-receiver family**
  (`docs/known-issues/natives-loop-carried-stale-receivers-20260907.md`,
  swept today, 09-07, 52 sites fixed). That family is Rust-side native
  method code holding an `ObjectRef` across a GC-capable call inside a loop.
  This crash's fault is in **JIT-compiled code** (`external/jit` frames in
  the native backtrace, and the crash banner explicitly reports "jit:
  faulting pc not attributed to a compiled method" only because
  `CRATONVM_DBG_JIT_NAMES` wasn't armed — the surrounding frames ARE
  attributed to `exe+0x...` and `external/jit`), not a native-builtins
  Rust function, so this is a different code layer than that sweep covers.
- **Not (confirmed) the netty Generational moving-young family** fixed
  today (`reference_the_netty_generational_moving_young_state_20260906.md`
  in the user's own memory — a blocked peer's stack/spill slots surviving a
  young relocation, fixed via `NativeSlotFixup` write-back on wake). The
  mechanism shape (a stale reference surviving a young-gen event) is
  superficially similar, and worth checking against, but this run's young
  generation did **zero moving cycles** (`gc young-gen actual: 0 moving
  cycle(s)`) — every young collection in this process's lifetime took the
  non-moving fallback path, so a relocation-write-back defect specifically
  cannot be the cause here; if this is the same family, the trigger has to
  be the non-moving sweep path's own bookkeeping, not object relocation.

## What is suspicious and unexplained

- The faulting access is a clean, deliberate-looking indexed load
  (`[rbx+r13*8]`, `r13=1` — the second array-style slot), not a wild jump —
  consistent with a valid-shaped array/object access through a base pointer
  (`rbx`) that is itself garbage or already-freed, rather than a
  corrupted instruction stream.
- The two `unlisted all-zero span … skipped, not freed` warnings from the
  non-moving sweep, seconds before the crash, are in the same young-gen
  bookkeeping area exercised by the fallback. Whether an "unlisted" span
  (one the sweep's own free-list didn't know about) can leave a stale
  pointer reachable is not established here — flagged as the most direct
  lead for whoever picks this up.
- `main-vm` thread, `Java frames ... published at the last blocking/safepoint
  deposit — may lag the faulting instruction` — the reported Java stack
  (JUnit Platform launcher internals, `SbRunner.main`) is almost certainly
  stale relative to the actual fault site inside compiled Jersey/Spring code;
  don't read it as the crash location.

## Next steps (not done here)

1. Re-run this one class alone (not as part of the 1975-class sweep) under
   `-XX:+UseGenerationalGC` with `CRATONVM_DBG_JIT_NAMES=1` to attribute the
   faulting pc to a real JIT method name, and `CRATONVM_SYMBOLIZE=<the exe+0x
   RVAs from the native frames>` against this exact binary
   (`target-springboot-3gc-v2/release/cratonvm.exe`) to symbolize the full
   native stack.
2. Check whether it reproduces at all outside a large concurrent sweep —
   this crash happened during a `-Parallel 4` run of the full 1975-class
   suite; a single-class isolated repro would settle whether host/scheduling
   contention is a factor the way it has been for other findings this
   session, or whether it is deterministic.
3. If it reproduces, bisect `--nojit` to separate a JIT-codegen cause from a
   GC/heap-bookkeeping one — the crash sits exactly on that boundary
   (JIT-compiled caller, GC fallback context) and the existing docs above
   only rule out specific OTHER mechanisms at that boundary, not this one.

## Repro

Not yet isolated. The class list and binary:

```bash
cd apps/spring-boot-suite-runner
# module/spring-boot-security, class:
#   org.springframework.boot.security.autoconfigure.actuate.web.servlet.JerseyEndpointRequestIntegrationTests
# binary used: C:/craton/CratonVM/target-springboot-3gc-v2/release/cratonvm.exe (dev tip 4f2ed2687)
```
