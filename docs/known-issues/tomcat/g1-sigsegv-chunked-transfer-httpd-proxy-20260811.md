# G1-only SIGSEGV in `TestChunkedTransferEncodingWithProxy`, in a process that evacuated 2128 regions without one walk break

| | |
|---|---|
| **Status** | OPEN — one crash in one full-suite run. Not the defect retired as `fixed-suite-bugs/tomcat/g1-sigsegv-unguarded-callee-jit-frame-FIXED.md`; see "Why this is not that". |
| **Discovered** | 2026-08-11, complete 651-class Tomcat suite, `-XX:+UseG1GC`, 2 workers, run concurrently with a default-GC and a ZGC arm (the 2026-08-10 shape). |
| **Frequency** | 1 of 651 classes, 1 of 1 runs. The same class **passes** under the default collector (163.8 s) and under ZGC (155.8 s) in the same three-way run, and its seven `integration.httpd` siblings all pass under G1 in 7–19 s. |
| **History** | This class was `HANG` (TIMEOUT 300 s) under G1 on 2026-08-10, so the crash may have been there all along behind a timeout. It has never been seen to crash on any other collector. |

## The dump

```
#  EXCEPTION_ACCESS_VIOLATION (SIGSEGV) (0xC0000005) at pc=0x00007FF710EA1C40
#  Faulting access: read at address 0x00000231D8BF0000
#  thread: "http-nio-127.0.0.1-auto-1-exec-1"
#  faulting RVA: 0x2F1C40
#  gc collector: g1
#  jit: guarded compiled frames live process-wide: YES (quiescence depth=4)
#  jit: 383 compiled code range(s), cache generation 733
#  jit: faulting pc not attributed to a compiled method
```

The faulting address is exactly page-aligned (`…0000`), the read is from
interpreter/runtime code (`exe+0x2F1C40`) called from JIT frames, and
`rsi=0x0000000041414141` — `AAAA` — is sitting in an argument register.

Java frames at the last deposit, innermost last:

```
… NioEndpoint$SocketProcessor.doRun → Http11Processor.service → CoyoteAdapter.service
… StandardWrapperValve.invoke → ApplicationFilterChain.doFilter → HttpServlet.service
… TomcatBaseTest$SnoopServlet.service
… Http11InputBuffer.fill → NioEndpoint$NioSocketWrapper.fillReadBuffer
```

Note the deposit is stale by construction (it is published at the last
blocking/safepoint point), so the bottom two frames are where the thread last
blocked on a socket read, not necessarily where it faulted.

## Why this is not the defect retired on 2026-08-11

That page's mechanism was an unaligned G1 TLAB carve: a linear walk of a pinned
Eden region arrives at bytes no filler/skip-span/object covers, reads them as an
all-zero 16-byte object, desyncs, and abandons the walk — after which heap
references past that offset are never rewritten by the pause.

In **this** process, with `CRATONVM_DBG=g1-dbg-reach` on:

| | |
|---|---|
| `[g1][FREED]` (regions evacuated) | **2128** |
| `[g1][PHASES]` (pauses) | 4 |
| `[g1][WALKBRK]` / `[g1][DESYNC-COVER]` | **0** |

So the collector really did move a great deal in this process, and the walk
never broke once. Across the whole 651-class G1 arm the count is the same:
**zero** walk breaks, in 135 processes whose stderr carries `[g1][FREED]`.

## Do not read the `young-gen` lines in this dump

The dump above (taken before the crash-reporter fix landed in this branch)
also carries:

```
#  gc young-gen policy: moving (Cheney young copy)
#  gc young-gen actual: 0 moving cycle(s), 0 cycle(s) diverted to the NON-MOVING sweep
#  gc young-gen last incomplete-coverage reason: innermost-rbp-belongs-to-unguarded-callee
```

The first two lines are **generational-collector state, printed under G1**.
`record_moving_young_cycle` and `record_moving_young_coverage_fallback` are each
called from exactly one place, `gen_heap.rs`, and `moving_young_enabled()` is
read nowhere in `g1.rs` — so under G1 both counters are zero *by construction*
and the policy line names a collector that is not running. `gc_state_lines_for`
now omits them for any non-generational collector; a fresh dump will not have
them. The incomplete-coverage reason line is genuine (`g1.rs` does call
`mark_moving_young_coverage_incomplete_because`) but it is a **root-coverage**
statement, not a moving-young one.

Misreading exactly these three lines together is how the retired page got its
title. Do not repeat it here.

## What is actually worth trying

1. **Reproduce.** One occurrence in one run. Re-run this class alone under G1,
   then under G1 with suite-level load. The retired page's own experience is
   that these Tomcat crashes do not reproduce standalone or at 4-way
   concurrency, so budget for the full-suite shape.
2. **`--nojit` under G1.** The faulting pc is runtime code entered from JIT
   frames and the dump reports 383 live compiled ranges. `--nojit` is the
   cheapest lever that either keeps or removes the whole JIT surface.
3. **`CRATONVM_DBG_JIT_NAMES=1`** so the next dump attributes the faulting pc
   to a compiled method instead of "not attributed".
4. **The httpd fixture.** This is one of the `integration.httpd` proxy tests;
   `apps/tomcat-suite-runner/setup-httpd-windows.ps1` provisions them. Confirm
   the fixture is healthy before treating a chunked-transfer test's crash as a
   pure VM defect — the seven siblings passing in 7–19 s while this one runs
   148 s says this class is doing something the others are not.

`0x41414141` in `rsi` is worth a second look: nothing in this fixture obviously
writes `AAAA`, so it is more likely a partially-overwritten or
never-initialised slot than an actual payload.
