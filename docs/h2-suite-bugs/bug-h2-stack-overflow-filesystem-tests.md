# H2 — `EXCEPTION_STACK_OVERFLOW` in file-system / poweroff tests

## Status
**OPEN** — root cause not yet isolated.

## Severity
**HIGH** — fatal process crash (or parent hang on a crashed child).

## Affected test classes (mem config)
| Class | Sweep result | Notes |
|-------|--------------|-------|
| `org.h2.test.poweroff.TestReorderWrites` | CRASH | overflow in `main-vm` thread |
| `org.h2.test.synth.TestDiskFull` | HANG | overflow (parent waits on crashed child) |
| `org.h2.test.unit.TestFileLockProcess` | HANG | spawns child VM that overflows |
| `org.h2.test.unit.TestSampleApps` | HANG | overflow |

All four fault at the **same** code location:
```
# A fatal error has been detected by the CratonVM Runtime Environment:
#  EXCEPTION_STACK_OVERFLOW (0xC00000FD) at pc=0x00007FF65C12E34E
#  faulting RVA: 0x81E34E
thread 'main-vm' (...) has overflowed its stack
```
Memory near the faulting code decodes to UTF-8 class/method names
`java/util/AbstractCollection`, `java/util/ArrayList$Itr`, `hasNext`,
`...ExtNext...` — suggesting the recursion runs through collection iteration.

## HotSpot behavior
PASS — all four pass quickly on HotSpot. So this is unbounded/too-deep
recursion specific to CratonVM, not a legitimately deep H2 call chain.

## Hypotheses (to verify)
- A CratonVM native or interpreter path that re-enters itself (mutual
  recursion between a native and the bytecode it invokes, e.g. an
  `AbstractCollection`/iterator native that calls back into the same virtual
  method).
- H2's recorded/wrapped file systems (`FilePathRec`, `FilePathReorder`,
  disk-full wrapper) layering `FilePath` delegations that CratonVM fails to
  bound, producing genuine deep recursion that overflows a smaller-than-HotSpot
  native stack.

## Next steps
- Re-run `TestReorderWrites` under `target/release-with-debug` and feed the
  `exe+0x` RVAs (incl. `0x81E34E`) back through `CRATONVM_SYMBOLIZE` to get the
  recursive frame (see `apps/h2database/h2/capture-h2-segv-rwd.ps1`).
- Check whether raising the VM thread stack size merely defers the crash
  (genuine deep recursion) or whether the recursion is unbounded (a VM bug).
