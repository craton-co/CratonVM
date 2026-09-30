// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Ending the process from a thread that has run Java.
//!
//! On Windows `std::process::exit` is `ExitProcess`: it kills every other
//! thread, then runs the CALLING thread's Rust thread-local destructors. A
//! Java thread's thread-locals include the collector's barrier buffers, whose
//! `Drop` takes locks collector threads also take; a killed holder never
//! releases them, and the exit hangs for ever
//! (`common-w10v-uncaught-error-exit-can-hang-in-process-exit-FIXED-20260923.md`).
//!
//! [`exit_process`] runs `ExitProcess` on a fresh thread whose thread-locals
//! hold nothing of the VM, and parks the caller; `ExitProcess` then kills the
//! parked caller without running its destructors. This is the shape of the
//! launcher's normal exit (the last thread to run is the launcher). Every
//! library's `DLL_PROCESS_DETACH` still runs, unlike `TerminateProcess`.
//!
//! Elsewhere `exit` kills no thread first, so it cannot hang this way, and
//! this is plain `std::process::exit`.

/// End the process with `code`. Flush whatever must survive BEFORE calling
/// this; it never returns.
pub fn exit_process(code: i32) -> ! {
    #[cfg(windows)]
    {
        let handed_off = std::thread::Builder::new()
            .name("cratonvm-exit".into())
            .stack_size(256 * 1024)
            .spawn(move || std::process::exit(code))
            .is_ok();
        if handed_off {
            loop {
                std::thread::park();
            }
        }
        // The spawn failed (out of threads or memory): exit here, as before.
    }
    std::process::exit(code)
}
