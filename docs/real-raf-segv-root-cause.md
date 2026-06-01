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

Part 1 closes a real latent cross-thread reclamation gap, **but it does NOT fix
the avrora real-RAF SEGV** (see below). Keep it on its own correctness merits.

## Part 2 — avrora real-RAF SEGV is NOT a GC reclamation/relocation bug

The earlier "GC×JIT roots" hypothesis is **disproven** by experiment (all on
`release-with-debug`, which reproduces the release crash WITH symbols):

| experiment | result |
|---|---|
| Part 1 applied (JIT roots in snapshot) | still SEGVs in `visit(CPI)`, obj_ptr=0x1 |
| instrumented `update_root_snapshot` | scan captures 20–37 JIT roots on ~74% of safepoints — the fix IS exercised |
| young-gen GC during the run | runs NON-MOVING while any thread is in JIT (no relocation); only ~1 young GC total |
| `CRATONVM_DBG_NO_CONC_GC=1` (disable old-gen concurrent GC) | still SEGVs |
| JIT disabled | no SEGV |
| synthetic RAF (incl. aggressive JIT forcing visit(CPI) to compile) | no SEGV |

So: relocation is prevented (non-moving while in JIT), reclamation now has the
JIT roots (part 1) AND the old-gen collector can be off — yet it still crashes.
GC is **ruled out**. The earlier "debug build is fixed by part 1" conclusion was
a **timing artifact**: part 1 adds a conservative JIT-frame scan to every
safepoint, which slows the (already byte-by-byte) real-RAF I/O enough that the
debug build times out in the *file-load* phase before ever reaching the
simulation phase where the crash lives. release is fast enough to reach it and
crashes identically.

### Real cause (refined): JIT clobbers local-0 (`this`) across a call in visit(CPI)

`LegacyInterpreter.visit(CPI)` (the real bytecode, javap-confirmed) is:

```
0:   aload_0; aload_0; getfield pc; iconst_2; iadd; putfield nextPC  // this.nextPC = this.pc+2
15:  ... invokevirtual getRegisterByte  // -> low(); a CALL
...  // flag computation into locals 6..11
160: aload_0; <nested ifeq/ifne/goto -> 0|1>; putfield H:Z          // this.H = bool
199: putfield C:Z ; 205: putfield N:Z ; ...                          // more flag stores
```

Decisive evidence from `CRATONVM_DBG_JIT_PUTFIELD`: the FIRST non-canonical
receiver is `field_index=11` (a flag field, stored at offset ~160, AFTER the
call), NOT the `nextPC` putfield (field #6, offset 7, BEFORE the call, same
`aload_0 this`). Since the one-shot did not fire on `nextPC`, `this` was VALID at
offset 7 and CORRUPTED to `0x1` by offset 160 — i.e. **`this` (local 0) is
clobbered across the `getRegisterByte`→`low()` call**. The clobber value `0x1`
is a boolean/small-int. This is a JIT **register/spill preservation** bug around
a method call, NOT GC and NOT the putfield codegen itself.

Suspected mechanism (Windows x64): the JIT maps locals to callee-saved GPRs and
spills register-resident locals to frame slots before a safepoint call
(`emit_pre_safepoint_spill`). A frame-layout overlap — e.g. local-0's spill slot
falling in the 32-byte shadow space the callee may write, or a `used_callee_saved`
gap so a callee clobbers a callee-saved reg holding `this` — would corrupt
local 0 across the call. Why only real-RAF: `visit(CPI)` is JIT-compiled only
when the real ELF program contains CPI instructions; synthetic RAF feeds
different bytes (different simulated program), so `visit(CPI)` never compiles —
which is exactly why aggressive-JIT + synthetic does NOT reproduce.

Reproduction status: NOT minimally reproducible yet — four faithful Java
reconstructions (`scratch/cpi/Cpi*.java`: the putfield pattern, the
getRegisterByte/low call, the visitor invokeinterface double-dispatch, and a
register-pressure-heavy callee) all produce CORRECT results on CratonVM. The bug
needs the exact register allocation / frame offsets the real (large) method
produces. Next steps: (1) instrument the JIT to dump, for `visit(CPI)`, which
storage holds local 0 and whether it is in `used_callee_saved` / its spill-slot
offset vs the shadow-space range; (2) or pin local 0 to a non-clobbered slot and
confirm the crash disappears, then fix the frame-layout/preservation gap.

**NOT acceptable** (no-mask rule): a non-canonical-receiver guard in
`jit_putfield_int` to skip/deopt the write.

Reproduce: `CRATONVM_REAL_RAF=1 [CRATONVM_DBG_JIT_PUTFIELD=1] cratonvm --jar
dacapo.jar avrora -s small`; symbolize `hs_err` RVAs with `CRATONVM_SYMBOLIZE=...`
on the same binary (use `--profile release-with-debug` for symbols).
