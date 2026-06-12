# Bug 04 — SIGSEGV (EXCEPTION_ACCESS_VIOLATION) in `org.apache.kafka.common.memory`

**Severity:** Critical — native access violation crashes the process.
`--nojit`, so interpreter-path. HotSpot runs the package clean.

**Symptom:**
```
#  EXCEPTION_ACCESS_VIOLATION (SIGSEGV) (0xC0000005) at pc=0x00007FF6...
#  Faulting access: read at address 0x000000001CB99C90
```

The `common.memory` tests exercise `MemoryPool` / `SimpleMemoryPool` and direct
`ByteBuffer` allocation. The faulting read address (`0x1CB99C90`) is a low,
non-heap address — consistent with a mis-decoded object/array pointer or an
off-heap (DirectByteBuffer / Unsafe) base that was computed incorrectly.

## Next steps
- Re-run with `CRATONVM_SYMBOLIZE` (release-with-debug) to symbolicate the faulting
  pc and the VEH backtrace.
- Narrow to the specific test class (bisect the package) and minimal repro
  (likely a DirectByteBuffer / Unsafe.copyMemory or MemorySegment path).

## Status
- [x] Reproduced (package `common.memory`, `--nojit`).
- [ ] Symbolicated / root-caused / fixed (open).
