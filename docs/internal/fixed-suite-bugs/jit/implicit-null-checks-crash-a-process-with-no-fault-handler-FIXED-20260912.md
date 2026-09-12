# FIXED: implicit null checks crashed a process that had no fault handler installed

**Status: FIXED 2026-09-12.** Found while verifying the 2026-09-12 JIT review
fixes on Windows. It predates the review: the same failure reproduces on the
review's base commit.

## Symptom

`vm/tests/jit_local_exception_handler_tests.rs`
`test_osr_loop_does_not_rerun_iterations_on_an_implicit_npe` killed its test
process with `STATUS_ACCESS_VIOLATION` (`0xC0000005`). There was no crash report,
no `hs_err_pid<pid>.log`, and no stderr banner. The other 19 tests in the binary
passed.

- `CRATONVM_JIT_IMPLICIT_NULL_CHECK=0` made it pass.
- `CRATONVM_JIT_OSR=0` made it pass.
- A 64 MiB test-thread stack, `CRATONVM_BG_COMPILE=0` and disabling the stack
  bang did not.
- Probe output added at the top of the vectored exception handler never printed,
  even though the handler is supposed to see every access violation.

## Cause

An implicit null check has two halves:

1. **The compiler elides the receiver check.** It emits nothing and records the
   receiver dereference's pc (`x64::Compiler::emit_trusted_oop_receiver_check_at`).
2. **A process-wide fault handler recovers the fault.** A Windows vectored
   exception handler, or the Unix `SIGSEGV` action, calls
   `implicit_null::recover` and resumes at the slow path, which raises the NPE.

Only `crash_handler::install_hardware_fault_handler` installs the second half.
`vm-cli` and `libcratonvm` call it at startup. `Vm::new` does not.

In a test process the only other caller is the harness shim in `vm/src/lib.rs`,
and that shim is `#[cfg(all(test, windows))]`. It exists only in the vm crate's
own unit-test binary. Every `vm/tests` integration binary links the ordinary vm
library, so no handler was ever installed there.

The first half was gated on `implicit_null::enabled()` alone, which is on by
default. So the compiler elided checks that nothing could recover. In this test,
`runNpe` is OSR-compiled and dereferences a null `Cell`. The fault found no
handler, and the OS terminated the process.

The OSR condition only decides whether `runNpe` runs compiled before its null
cell comes up. With OSR off, the method runs interpreted until method-entry
tier-up, and the NPE is raised by the interpreter.

## Fix

Elision now requires the handler.

- **Installer side.** `implicit_null::note_fault_handler_installed` records that
  a recovering handler exists. `windows_fault::install` calls it once
  `AddVectoredExceptionHandler` returns a handle. The Unix `SIGSEGV` installation
  in `install_signal_handlers` calls it once `sigaction` succeeds.
- **Compiler side.** `implicit_null::active()` is `enabled() &&
  fault_handler_installed()`. `emit_trusted_oop_receiver_check_at` asks
  `active()`. Until a handler is noted, every site keeps its explicit check,
  which is exactly the flag's off arm.
- **The static ratchet.** The flag is one new static. The module's four counter
  statics became a single `COUNTERS` array, so the statics count went down.

`vm-cli`, `libcratonvm` and the vm crate's unit tests all install the handler
before they compile anything, so implicit null checks stay in effect there.
Integration binaries and embeddings that never install one get explicit checks
instead of a process kill.

## Regression coverage

- `test_osr_loop_does_not_rerun_iterations_on_an_implicit_npe` passes in the
  integration binary.
- `jit/src/implicit_null.rs`:
  `elision_needs_a_recovering_fault_handler_as_well_as_the_flag`.
- `jit/src/x64/loop_unroll_admission.rs`
  `every_unrolled_copy_of_an_implicit_null_check_is_registered` marks a handler
  installed before it counts sites. It compiles but never executes a null
  receiver.
