> **STATUS (2026-10-19, gcj j3/rf3): OPEN -- found by reading (the `pf_gr_cg_r3` silent `rc=1` death); opt-in diagnostic built (`CRATONVM_DBG_UNHANDLED_EXCEPTION_REPORT`, commit `eb009ff423`), the default is unchanged.**

# Windows: an unhandled exception outside the crash handler's fatal list ends the VM with nothing on stderr

## What the code does (file:line on `c7503ea6e1`)

`vm/src/runtime/crash_handler.rs` `windows_fault`:

- `install` (~1710) registers ONE vectored exception handler (`vectored_handler`, ~982). It reports
  (banner, PC, backtrace, `hs_err_pid*.log`) only the codes `is_fatal` (~969) lists:
  `0xC0000005`, `0xC0000006`, `0xC000001D`, `0xC0000096`, `0xC0000094`, `0xC00000FD`,
  `0xC0000409`. Every other code returns `EXCEPTION_CONTINUE_SEARCH` unreported (correctly: a
  vectored handler sees first-chance exceptions, Rust unwinds included).
- No `SetUnhandledExceptionFilter` is installed outside the test-only `harness_exit_shim`
  (`vm/src/lib.rs` ~343, `#[cfg(all(test, windows))]`).

So an exception that nobody handles and that is not on the list ends the process with the
exception code as its exit status and an empty stderr. Examples: `0x80000001`
`STATUS_GUARD_PAGE_VIOLATION` (a thread reading ANOTHER thread's stack guard page: the kernel only
grows the faulting thread's own stack), `0x80000002` misalignment, `0x80000003` breakpoint,
`0xC0000095` integer overflow, `0xC0000374` heap corruption when not fast-failed, `0xC0000017`
no-memory from a `HEAP_GENERATE_EXCEPTIONS` heap.

## Why it matters for the battery

Git Bash maps a native exit status below `0xC0000000` to its low byte, so `0x80000001` reads as
`rc=1` -- with no stdout, no stderr and no `CRATONVM_GC_STATS` exit block: exactly the
`pf_gr_cg_r3` record (`docs/known-issues/gc/gcj-j2rf2-two-empty-stdout-leads-a-hotspot-class-load-flake-and-a-silent-exit-1-20261019.md`).
Not proven to be its cause; it is the one VM-side mechanism found by reading that produces that
shape (every `process::exit` / `abort` path prints first, the listed faults print a banner).

## What would close it

1. Measure: the battery now records the full code (`<row>.rcfull`); re-run `gcj3rf3_rc1_*` (j3-rf3
   report) until a repetition dies; its `.rcfull` names the code.
2. Default the filter ON (with `=0` kill) once it has run clean on the battery: it only adds a line
   on stderr on a path that is already a crash, and chains to any previous filter.
3. If the code is `0x80000001`: find the reader of another thread's stack (the conservative peer
   scans already skip `PAGE_GUARD` regions, `vm/src/jit/xt_root_scan.rs` ~3385; check every other
   cross-thread stack read, e.g. the interpreter-frame scans of parked threads under `--nojit`).

## Rows

`gcj3rf3_rc1_*` in `docs/internal/gc-round-gcj-20261019/j3-rf3-report.md`.
