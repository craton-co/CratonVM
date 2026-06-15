# H2 — `TestUtils` fatal `EXCEPTION_ACCESS_VIOLATION` (SIGSEGV)

## Status
**OPEN** — not yet isolated (needs symbolized backtrace).

## Severity
**HIGH** — fatal process crash.

## Affected test class
`org.h2.test.unit.TestUtils` (CRASH in both the baseline and post-fix sweeps —
pre-existing, unrelated to the repeat / BufferedReader fixes).

## Symptom
```
# A fatal error has been detected by the CratonVM Runtime Environment:
#  EXCEPTION_ACCESS_VIOLATION (SIGSEGV) (0xC0000005) at pc=0x00007FF7092E450F
#  thread: "main-vm"
#  faulting RVA: 0x1F450F
```
No Java output precedes the crash (faults early). The faulting RVA `0x1F450F`
is **distinct** from the file-system tests' stack-overflow site (`0x81E34E`),
so this is a different defect.

## HotSpot behavior
PASS.

## Context
`org.h2.test.unit.TestUtils` exercises `org.h2.util.Utils` helpers
(reflection/`newInstance`, `getProperty`, sorting/`MemoryUnmapper`-style memory
helpers, `getNonPrimitiveClass`, etc.). A native/unsafe memory path or a
reflection helper is the likely culprit.

## Next steps
- Re-run under `target/release-with-debug` and symbolize the `exe+0x` RVAs
  (incl. `0x1F450F`) via `CRATONVM_SYMBOLIZE` (see
  `apps/h2database/h2/capture-h2-segv-rwd.ps1`).
- Bisect `TestUtils` test methods to the one that faults.

## Repro
`java -cp temp;ext org.h2.test.RunOne org.h2.test.unit.TestUtils mem`
