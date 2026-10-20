// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT round 19 proposal wave 10, lane mon10: page
//! `docs/internal/jit-proposals/i76-L5-proposal-osr-loops-pay-the-monitor-helper-20261109-RETIRED-20261009.md`
//! suspected that an OSR body's `monitorenter` / `monitorexit` take the
//! `jit_monitor_enter` / `jit_monitor_exit` helper where a method-entry body
//! takes the inline thin-lock fast path (its hypothesis (1): javac's `dup`'d
//! receiver is not frame-homed in an OSR artifact).
//!
//! By reading, neither tier makes that distinction:
//!
//! * the single-pass tier compiles ONE artifact per method and publishes its
//!   loop headers in `osr_pc_to_native`; the OSR door (`compile_osr_body`)
//!   reaches the same `Compiler`, and `emit_monitor_on_stack_top` gives every
//!   register-resident receiver a frame home BEFORE it asks
//!   `emit_inline_thin_lock` (`x64/op_object.rs`), so the census reason
//!   `receiver-not-frame-homed` is unreachable for a `dup`'d copy. The thread
//!   comes from `jit_thread_slot_off`, which the OSR trampoline writes from its
//!   `thread_ptr` argument (`osr_entry.rs`), or from the `JIT_THREAD` mirror;
//! * the optimizing tier's `Op::MonitorEnter | Op::MonitorExit` arm calls
//!   `emit_monitor_fast_path` on the receiver's frame slot whatever door built
//!   the graph, with the thread from the `JIT_THREAD` mirror.
//!
//! These tests pin that by shape, on javac's bytecode of the page's loop: the
//! artifact that serves the OSR entry at the loop header carries the inline
//! `LOCK CMPXCHG [R11 + mark], ECX` at every monitor site, and
//! `CRATONVM_MONITOR_FASTPATH=0` (the positive control) removes all of them.
//! The held-monitor parity facts are untouched: no emitter changed.

use crate::r19_syncdoor3_tests::{method, row, wired_helpers};
use crate::{CachedBytecodeMethod, CompiledMethod};

/// `int loop(int n) { int i = 0; while (i < n) { synchronized (other) { o++; }
/// i++; } return o; }` (instance; cp 6 `other:Ljava/lang/Object;`, cp 8
/// `o:I`). Loop header at bci 2; javac's two catch-any rows `[14, 26) -> 29`
/// and `[29, 33) -> 29`, neither covering the header.
const SYNC_LOOP: [u8; 47] = [
    0x03, 0x3d, // 0: iconst_0; istore_2
    0x1c, 0x1b, 0xa2, 0x00, 0x26, // 2: iload_2; iload_1; if_icmpge 42
    // 7: aload_0; getfield other; dup; astore_3; monitorenter
    0x2a, 0xb4, 0x00, 0x06, 0x59, 0x4e, 0xc2, 0x2a, 0x59, 0xb4, 0x00, 0x08, 0x04, 0x60, 0xb5, 0x00,
    0x08, // 14: o++
    0x2d, 0xc3, 0xa7, 0x00, 0x0a, // 24: aload_3; monitorexit; goto 36
    // 29: astore 4; aload_3; monitorexit; aload 4; athrow
    0x3a, 0x04, 0x2d, 0xc3, 0x19, 0x04, 0xbf, 0x84, 0x02, 0x01, 0xa7, 0xff,
    0xdb, // 36: iinc 2, 1; goto 2
    0x2a, 0xb4, 0x00, 0x08, 0xac, // 42: aload_0; getfield o; ireturn
];

/// The loop header (the back edge's target): the OSR entry pc.
const HEADER: usize = 2;

/// The three monitor sites (enter, exit, the catch-any's exit).
const MONITOR_SITES: usize = 3;

fn sync_loop() -> CachedBytecodeMethod {
    let mut m = method(
        "loop",
        &SYNC_LOOP,
        vec![row(14, 26, 29, 0), row(29, 33, 29, 0)],
        3,
        5,
    );
    m.is_synchronized = false;
    m
}

fn compile(optimize: bool, edits: &[(&str, Option<&str>)]) -> Option<CompiledMethod> {
    let cached = sync_loop();
    let helpers = if optimize {
        // The optimizing lowering can read a nonzero fake as a table address
        // (lane handler7's note), so only what the monitor and field lowering
        // name is wired. SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every
        // field is a `usize`, so all-zero is a valid value; never run.
        let mut h: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
        h.monitor_enter = 0x5000_0000;
        h.monitor_exit = 0x5000_0040;
        h.monitor_block_offset_in_thread = 0x28;
        h.class_id_offset_in_obj = 8;
        h.getfield = 0x5000_0080;
        h.putfield_int = 0x5000_00C0;
        h
    } else {
        wired_helpers()
    };
    let fields = |cp: u16| -> Option<(usize, u8, Option<(u32, bool)>)> {
        match cp {
            6 => Some((2, b'L', None)),
            8 => Some((3, b'I', None)),
            _ => None,
        }
    };
    let direct = crate::DirectHelperTable {
        monitor_enter: 0x5000_0000,
        monitor_exit: 0x5000_0040,
        ..crate::DirectHelperTable::EMPTY
    };
    let verdicts = crate::JitVerdictRegistry::default();
    let retries = crate::jfr_compile_decision::DeferredNewRetries::default();
    let mut req = crate::compile_request::CompileRequest::new(
        &cached,
        &helpers,
        crate::compile_request::CompileRealm {
            verdicts: &verdicts,
            deferred_new_retries: &retries,
        },
    );
    req.cp_field_resolver = Some(&fields);
    req.direct_helpers = &direct;
    req.optimize = optimize;
    cratonvm_types::flags::with_thread_overrides(edits, || crate::try_compile_request(&req))
}

/// `LOCK CMPXCHG [R11 + MARK_WORD_OFFSET], ECX`: the thin path's CAS on the
/// mark word, emitted only by `runtime_lowering::emit_inline_thin_lock_with`.
fn inline_cas_count(cm: &CompiledMethod) -> usize {
    // Cast: pinned < 128 by the emitter's own const assertion.
    let mark = cratonvm_types::MARK_WORD_OFFSET as u8;
    let pat = [0xF0, 0x41, 0x0F, 0xB1, 0x4B, mark];
    cm.code_bytes()
        .windows(pat.len())
        .filter(|w| *w == pat)
        .count()
}

/// The single-pass artifact (the one the OSR door enters at the header)
/// takes every monitor of the loop inline; `CRATONVM_MONITOR_FASTPATH=0`
/// leaves only the helper calls.
#[test]
fn the_osr_entered_single_pass_loop_takes_its_monitors_inline() {
    let cm = compile(false, &[]);
    assert!(
        cm.is_some(),
        "the synchronized loop compiles on the single-pass tier"
    );
    let Some(cm) = cm else { return };
    assert!(
        cm.can_osr_enter(HEADER),
        "the loop header outside both catch-any rows is an OSR entry"
    );
    let sites = inline_cas_count(&cm);
    assert!(
        sites >= MONITOR_SITES,
        "every monitor site of the OSR-served artifact carries the inline CAS \
         (found {sites})"
    );

    let off = compile(false, &[("CRATONVM_MONITOR_FASTPATH", Some("0"))]);
    assert!(off.is_some(), "the helper-only control compiles too");
    let Some(off) = off else { return };
    assert_eq!(
        inline_cas_count(&off),
        0,
        "positive control: with the fast path off no site is inline"
    );
}

/// The optimizing tier inlines the same sites (here after its loop lock
/// coarsening, which keeps a thin pair per copy and per exit); the arm
/// (`Op::MonitorEnter | Op::MonitorExit`) does not ask which door built the
/// graph, so an OSR-door graph's monitors lower the same way.
#[test]
fn the_optimizing_loop_takes_its_monitors_inline() {
    let cm = compile(true, &[]);
    assert!(cm.is_some(), "the synchronized loop compiles");
    let Some(cm) = cm else { return };
    assert!(cm.used_ir_backend, "the optimizing tier takes the loop");
    assert!(
        inline_cas_count(&cm) >= 2,
        "an optimizing body's live monitor ops carry the inline CAS"
    );
    let off = compile(true, &[("CRATONVM_MONITOR_FASTPATH", Some("0"))]);
    assert!(off.is_some(), "the helper-only control compiles too");
    let Some(off) = off else { return };
    assert!(off.used_ir_backend);
    assert_eq!(inline_cas_count(&off), 0, "positive control");
}
