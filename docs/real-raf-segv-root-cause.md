# Real-bytecode RAF SEGV — precise root cause (2026-06-01)

This supersedes the earlier hypothesis in
`system-out-redirect-and-fos-buffering-fix.md` (§"why real-bytecode RAF SEGVs
avrora — the JIT re-entrancy UB"). **That hypothesis was wrong** and is
disproven below. The real cause is now pinned to a specific faulting
instruction, faulting function, faulting value, and triggering method.

## TL;DR

- The `jit_thread_mut` "aliasing &mut JvmThread borrow" debug assert is a
  **false positive**. The nested-dispatch re-entry is a sound *child* reborrow
  (the inner `&mut *ptr` descends from the outer `&mut` via the
  `set_jit_thread(thread)` cast). Proven: with the assert neutered, avrora
  drives 1115 nested borrows and **PASSES** with no SEGV. Fixed by making the
  borrow flag level-aware (commit "fix(jit): make jit_thread_mut borrow check
  level-aware").
- The actual real-RAF SEGV is a **JIT × GC root-corruption** bug, NOT the
  re-entrancy and NOT in the RAF/Cleaner code itself.

## How it was localized

Tooling added this session (Windows had ZERO hardware-fault diagnostics — a
SEGV died with empty stderr + bare `STATUS_ACCESS_VIOLATION`):

1. `install_hardware_fault_handler()` — a Windows vectored exception handler
   (`crash_handler.rs`) that on a fatal fault prints the faulting PC, access
   type + data address, thread, exe module base + faulting RVA, and a raw
   `RtlCaptureStackBackTrace` return-address list (robust on corrupted/JIT
   stacks where `Backtrace::force_capture` re-faults), written single-shot to
   `hs_err_pid<pid>.log` so it survives multi-threaded teardown races.
2. `CRATONVM_SYMBOLIZE=<rva,...>` startup hook — resolves exe-relative RVAs
   against the SAME binary's PDB via dbghelp `SymFromAddr` (set the symbol
   search path to the exe dir; `SymLoadModuleExW` the main module).
3. `CRATONVM_REAL_RAF=1` — gates OFF the synthetic `RandomAccessFile` natives in
   `native-io::register_io_extras_natives` and
   `phases_late::register_phase57_random_access_file`, so RAF runs real JDK
   bytecode (the reproduction switch).
4. `CRATONVM_DBG_JIT_PUTFIELD=1` — `jit_putfield_int` reports a non-canonical
   receiver (obj_ptr not 8-aligned / out of canonical range) with the
   containing JIT method via a save/restore `CURRENT_JIT_CALLEE` thread-local.

## The fault, exactly

```
EXCEPTION_ACCESS_VIOLATION (write) at core::ptr::write::<Value>+0x3
  called from  cratonvm_vm::jit::helpers::jit_putfield_int+0xFE
  called from  JIT-compiled code
on a worker thread ("Thread-2"/"Thread-3")
[JIT-PFI-BAD] obj_ptr=0x1 field_index=11 val=0x1  (write target 0xD9)
current_jit_callee = avrora/arch/legacy/LegacyInstrVisitor.visit(Lavrora/arch/legacy/LegacyInstr$CPI;)V
```

`jit_putfield_int` guards only `obj_ptr == 0`; here `obj_ptr == 0x1` (the
int/boolean `1`, NOT a heap pointer), so it computes `1 + HEADER_SIZE +
11*SLOT_SIZE = 0xD9` and `ptr::write`s a `Value::Int` there → SIGSEGV.

## Why it is GC × JIT, not a static miscompile and not RAF/Cleaner code

Decisive experiments (all on `avrora -s small`):

| config | JIT | RAF | result |
|---|---|---|---|
| default | on | synthetic | **PASS** |
| default, `-s default` | on | synthetic | **PASS** |
| aggressive JIT (threshold 20) | on | synthetic | **PASS** |
| `CRATONVM_DISABLE_JIT=1` | off | real | no SEGV (runs; avrora digest differs) |
| default | on | **real** | **SEGV** in `visit(CPI)`, obj_ptr=0x1 |

- JIT off ⇒ no crash ⇒ the fault requires JIT-compiled code.
- Synthetic RAF never crashes, even with aggressive JIT forcing `visit(CPI)` to
  compile ⇒ NOT a static codegen bug in `visit(CPI)`.
- avrora's simulated instruction stream is deterministic regardless of RAF, so
  the ONLY thing real-RAF changes is **host GC behavior**: the real RAF ctor
  allocates `FileDescriptor` + `Cleaner`/`FileCleanable`/`PhantomReference`,
  driving extra (STW, compacting) GCs.
- The crash is on a worker thread *running JIT-compiled `visit(CPI)`*, whose
  receiver (the interpreter object) is held in a JIT frame slot. The JIT
  compiler **does not emit precise oop maps** (see
  `conservative_roots::JitEntryGuard::enter_with_compiled` — falls back to
  conservative scanning). Under the extra real-RAF GC pressure, a relocation
  during `visit(CPI)` corrupts that receiver slot to `0x1`.

## The fix — part 1 DONE: cross-thread JIT roots in the snapshot

Commit "fix(gc): scan active JIT frames in the cross-thread root snapshot".

The STW collector reads each thread's roots via `collect_all_root_snapshots()`
(the only cross-thread path; `collect_roots`, which *did* scan JIT frames, runs
only on the current thread / in tests). But `update_root_snapshot`
(interpreter.rs) scanned only `thread.frames` + native pins — it NEVER scanned
the thread's active JIT spill region. So a worker thread's JIT-held receiver was
entirely absent from the snapshot the collector reads. Fix:
`update_root_snapshot` now also calls `scan_active_jit_frames` (it always runs on
the thread it snapshots, so the thread-local scan captures that worker's live
JIT frame).

**Verified:** with this fix, `CRATONVM_REAL_RAF=1` avrora **no longer SEGVs in a
debug build** (it advances all the way to real-RAF I/O); synthetic avrora still
PASSES in both debug and release (no regression).

## The fix — part 2 NOT done: STW marking freshness for in-JIT threads

A **release** build of `CRATONVM_REAL_RAF=1` avrora still crashes (debug does
not). The remaining gap: the collector defers *compaction* while any thread is
in JIT (`any_thread_in_jit`/`gc_must_defer`) but still *marks* — and marking
uses each thread's last-published snapshot. A worker thread actively executing
JIT-compiled code that has NOT parked at a safepoint since entering its current
JIT frame has a STALE snapshot (missing the current frame's JIT roots), so
marking reclaims the receiver. Part 1 fixes snapshot *content*; this is a
snapshot *freshness* / STW-coordination gap. Debug hides it (slower JIT, more
likely parked at a poll when GC fires); release exposes it.

Tractable completions (follow-up):
- Guarantee every in-JIT thread reaches a safepoint poll and republishes
  (`update_root_snapshot`, now JIT-aware) before STW marking proceeds — i.e. the
  collector must WAIT for all in-JIT threads to park, and JIT code must poll
  safepoints densely enough that it always can.
- OR have the STW collector scan each in-JIT thread's JIT stack directly from a
  per-thread (SP, JIT-chain) snapshot captured at the safepoint (the
  "future enhancement" named in `conservative_roots.rs`).

**NOT acceptable** (no-mask rule): a non-canonical-receiver guard in
`jit_putfield_int` to skip/deopt the write — that hides the GC corruption and
would silently produce wrong results.

Reproduce: `CRATONVM_REAL_RAF=1 [CRATONVM_DBG_JIT_PUTFIELD=1] cratonvm --jar
dacapo.jar avrora -s small`; symbolize `hs_err` RVAs with `CRATONVM_SYMBOLIZE=...`
on the same binary (use `--profile release-with-debug` for symbols).
