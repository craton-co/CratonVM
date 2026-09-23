// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Round 10, lane `earelock`: the SECOND gate that keeps the single-pass
//! backend's "Phase C" scalar-monitor relock machinery unreachable.
//!
//! `docs/internal/fixed-bugs/r10-ea-single-pass-monitor-scalar-relock-is-unreachable-FIXED-20260922.md`
//! opened on one gate: `x64/escape_analysis.rs::analyze_escapes` has no
//! `monitorenter`/`monitorexit` arm, so a `synchronized` receiver falls into
//! the catch-all and is escaped, and `plan_scalar_replacement`'s Phase C code
//! can never receive a candidate. That page's proposed fix is "add the arms".
//!
//! This test pins the fact that adding them would not be enough, because a
//! second gate — in a different file, on a different axis — closes on exactly
//! the same population first:
//!
//! ```text
//!   jit/src/x64/driver.rs
//!     let non_escaping_for_sr = if is_baseline || precise_exception_frames || … {
//!         &empty_non_escaping        // <- plan_scalar_replacement returns `empty`
//!     } else {
//!         &non_escaping_new
//!     };
//! ```
//!
//! `precise_exception_frames` is set (`jit/src/lib.rs::try_compile_inner`)
//! whenever `local_handler_reads_unsafe_local` is true for the method, i.e.
//! whenever any exception handler reads a local beyond `this` and the declared
//! parameters. javac's `synchronized (x) { … }` ALWAYS emits a catch-all
//! handler of the shape
//!
//! ```text
//!   astore <e> ; aload <mon> ; monitorexit ; aload <e> ; athrow
//! ```
//!
//! and `<mon>` is a compiler-synthesised temporary, allocated above every
//! declared parameter slot. So the handler always reads a non-parameter local,
//! `precise_exception_frames` is always true, and scalar replacement is
//! switched off wholesale for every method containing a javac `synchronized`
//! block — arms or no arms.
//!
//! `handler_resume_requires_precise_locals` is the public, VM-side view of the
//! same predicate (`jit/src/lib.rs`), delegating to the identical
//! `regalloc::handler_has_unsafe_local_read` dataflow that the compile gate
//! uses; asserting on it is asserting on the gate.
//!
//! Written by reading the code; this lane was not permitted to build or run
//! anything. The bytecode below is hand-assembled from the javac pattern.

use cratonvm_reader::attribute::ExceptionTableEntry;

/// The javac lowering of
/// `void m() { Foo f = new Foo(); synchronized (f) { f.x = 1; } }`.
///
/// `monitor_local_load` is the opcode at pc 23 (inside the handler): `aload_2`
/// (`0x2c`) in the real javac output, which reads the synthetic monitor
/// temporary. The control below passes `aload_0` (`0x2a`) instead, which reads
/// `this` — a declared parameter slot — so that the two runs differ in exactly
/// one byte and the cause is isolated.
fn synchronized_block_code(monitor_local_load: u8) -> Vec<u8> {
    vec![
        0xbb,
        0x00,
        0x01, // 0: new #1
        0x59, // 3: dup
        0xb7,
        0x00,
        0x02, // 4: invokespecial #2 <init>()V
        0x4c, // 7: astore_1             -> f in local 1
        0x2b, // 8: aload_1
        0x59, // 9: dup
        0x4d, // 10: astore_2            -> local 2 = the synthetic monitor temp
        0xc2, // 11: monitorenter
        0x2b, // 12: aload_1             -- protected range starts
        0x04, // 13: iconst_1
        0xb5,
        0x00,
        0x03, // 14: putfield #3
        0x2c, // 17: aload_2
        0xc3, // 18: monitorexit
        0xa7,
        0x00,
        0x08,               // 19: goto 27 -- jump over the handler
        0x4e,               // 22: astore_3            -- handler entry: stash the exception
        monitor_local_load, // 23: aload_2 (or aload_0 in the control)
        0xc3,               // 24: monitorexit
        0x2d,               // 25: aload_3
        0xbf,               // 26: athrow
        0xb1,               // 27: return
    ]
}

/// The two catch-all entries javac emits for one `synchronized` block: one over
/// the body, one over the handler's own `monitorexit` (so a throw there
/// re-enters the same handler).
fn synchronized_block_exception_table() -> Vec<ExceptionTableEntry> {
    vec![
        ExceptionTableEntry {
            start_pc: 12,
            end_pc: 19,
            handler_pc: 22,
            catch_type: 0,
        },
        ExceptionTableEntry {
            start_pc: 22,
            end_pc: 25,
            handler_pc: 22,
            catch_type: 0,
        },
    ]
}

/// The finding.
///
/// `handler_resume_requires_precise_locals` answers "does resuming one of this
/// method's handlers need locals beyond `this` + the declared parameters?" —
/// `true` means the compile gate sets `precise_exception_frames`, which in
/// `x64/driver.rs` replaces the escape-analysis result with the EMPTY set
/// before `plan_scalar_replacement` ever sees it.
///
/// For `void m()` on an instance class the safe set at handler entry is
/// `{ slot 0 }` (`this` alone). The handler's first instruction, `astore_3`,
/// adds slot 3; its second, `aload_2`, reads slot 2, which is in neither. The
/// dataflow therefore reports an unsafe read at pc 23 and returns `true` — so
/// this assertion is `assert!(…)`, not `assert!(!…)`.
#[test]
fn a_javac_synchronized_block_forces_precise_exception_frames() {
    let code = synchronized_block_code(0x2c); // aload_2 — the monitor temp
    let table = synchronized_block_exception_table();
    assert!(
        cratonvm_jit::handler_resume_requires_precise_locals(
            &code,
            code.len(),
            &table,
            "()V",
            false, // instance method: `this` occupies slot 0
        ),
        "javac's synchronized-block handler reads the synthetic monitor local \
         (slot 2), which is not `this` and not a declared parameter. That sets \
         `precise_exception_frames`, and `x64/driver.rs` then hands \
         `plan_scalar_replacement` an EMPTY non-escaping set -- so Phase C \
         stays unreachable for this method even if `analyze_escapes` were \
         taught exact monitorenter/monitorexit arms. If this assertion ever \
         fails, re-read \
         docs/internal/fixed-bugs/r10-ea-single-pass-monitor-scalar-relock-is-unreachable-FIXED-20260922.md: \
         one of its two gates has moved."
    );
}

/// The control, differing from the test above in exactly one byte.
///
/// With the handler reading `this` (slot 0, always in the initial safe set)
/// instead of the monitor temporary, the same dataflow walks
/// `astore_3 → aload_0 → monitorexit → aload_3 → athrow` without ever reading
/// an unassigned slot, and `athrow` terminates the path with no successors. So
/// the predicate is `false` here, which is what proves the `true` above is
/// caused by the monitor local and not by the exception table, the `athrow`,
/// or the method shape.
#[test]
fn the_same_handler_reading_only_this_does_not_force_precise_frames() {
    let code = synchronized_block_code(0x2a); // aload_0 — `this`
    let table = synchronized_block_exception_table();
    assert!(
        !cratonvm_jit::handler_resume_requires_precise_locals(
            &code,
            code.len(),
            &table,
            "()V",
            false,
        ),
        "control: a handler that reads only `this` and the local it just \
         stored is resumable from the parameters alone, so the gate does not \
         fire -- the one-byte difference from the test above is the whole cause"
    );
}

/// And the same bytecode with no exception table at all is not gated either —
/// the predicate loops over the table and returns `false` for an empty one.
///
/// This is the shape the known-issue page's remaining escape hatch names:
/// hand-written or non-javac bytecode that locks and unlocks without
/// structured-locking protection. It is the only population for which adding
/// `monitorenter`/`monitorexit` arms to `analyze_escapes` could change an
/// outcome, and it is not what any Java compiler emits.
#[test]
fn unprotected_monitor_bytecode_is_the_only_population_the_gate_misses() {
    let code = synchronized_block_code(0x2c);
    assert!(
        !cratonvm_jit::handler_resume_requires_precise_locals(&code, code.len(), &[], "()V", false,),
        "with no exception table there is no handler to be unsafe, so the \
         gate is open -- which is precisely why the remaining Phase C \
         escape hatch is bytecode no javac produces"
    );
}
