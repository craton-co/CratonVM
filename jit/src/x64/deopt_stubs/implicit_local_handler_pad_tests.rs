//! The implicit half of the compiled-local-handler feature
//! (`CRATONVM_JIT_IMPLICIT_LOCAL_HANDLERS`, 2026-09-20): a bounds check, an
//! inline null check or a zero-divisor guard whose bci this method can
//! catch asks `jit_local_handler_lookup` before running its epilogue.
//!
//! These assert the BYTES, because the behavioural half
//! (`vm/tests/jit_implicit_local_handler_tests.rs`) needs a built binary
//! and a JDK and skips itself without them.

use crate::x64::{Compiler, RAX};

/// A bare `Compiler` with a wired `throw_aioobe` / `local_handler_lookup`
/// and the exception table the test wants. Shaped like
/// `flag_and_header_contracts::bounds_check_test_compiler`, which it
/// deliberately does not reuse: that fixture is the no-table control and
/// must keep emitting exactly what it emitted before this feature.
fn pad_compiler(table: Vec<(usize, usize, usize, &'static str)>) -> Compiler {
    let alloc_result = crate::regalloc::RegAllocResult {
        assignments: Vec::new(),
        xmm_assignments: Vec::new(),
        used_callee_saved: Vec::new(),
        used_xmm_regs: Vec::new(),
        block_live_in: Vec::new(),
    };
    let mut helpers = cratonvm_jit_api::JitRuntimeHelpers::default();
    // Any non-zero, in-reach addresses: the pads only have to be
    // emittable, they are never executed here.
    helpers.throw_aioobe = pad_compiler as usize;
    helpers.local_handler_lookup = pad_compiler as usize + 0x40;
    let mut c = Compiler::new(
        "implicit-local-handler-pad-test".to_string(),
        crate::ExecutableBuffer::new(8192).expect("test executable buffer"),
        0,
        0,
        8,
        // `needs_heap`: the probe refuses a frame with no VM pointer, so a
        // fixture that wants to see it emitted has to reserve the slot.
        true,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        alloc_result,
        false,
        helpers,
        0,
        false,
        false,
        false,
        false,
        false,
        Vec::new(),
    );
    c.local_handler_table = table;
    c.local_handler_class_id = 7;
    c
}

/// [`pad_compiler`] with `needs_heap` false: a method whose only fallible
/// sites are a primitive array access and an `idiv` reserves no VM-pointer
/// slot, which is the shape the probe's own guard exists for.
fn heapless_pad_compiler(table: Vec<(usize, usize, usize, &'static str)>) -> Compiler {
    let alloc_result = crate::regalloc::RegAllocResult {
        assignments: Vec::new(),
        xmm_assignments: Vec::new(),
        used_callee_saved: Vec::new(),
        used_xmm_regs: Vec::new(),
        block_live_in: Vec::new(),
    };
    let mut helpers = cratonvm_jit_api::JitRuntimeHelpers::default();
    helpers.throw_aioobe = pad_compiler as usize;
    helpers.local_handler_lookup = pad_compiler as usize + 0x40;
    let mut c = Compiler::new(
        "implicit-local-handler-heapless-test".to_string(),
        crate::ExecutableBuffer::new(8192).expect("test executable buffer"),
        0,
        0,
        8,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        alloc_result,
        false,
        helpers,
        0,
        false,
        false,
        false,
        false,
        false,
        Vec::new(),
    );
    c.local_handler_table = table;
    c.local_handler_class_id = 7;
    c
}

/// `MOV RAX, imm64` with the deopt sentinel — what the miss edge owes the
/// epilogue once the probe's `CALL` has clobbered RAX.
fn sentinel_reload() -> Vec<u8> {
    let mut v = vec![0x48, 0xB8];
    v.extend_from_slice(&(i64::MIN as u64).to_le_bytes());
    v
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    needle.len() <= hay.len() && hay.windows(needle.len()).any(|w| w == needle)
}

/// The control. No exception table ⇒ no site ⇒ the pad is byte-for-byte
/// the pre-feature pad: the throw helper's sentinel goes straight to the
/// epilogue and RAX is never reloaded.
#[test]
fn a_method_with_no_exception_table_emits_the_pre_feature_bounds_pad() {
    let mut c = pad_compiler(Vec::new());
    c.emit_bounds_check(0);
    let start = c.buf.pos();
    c.emit_bounds_check_stubs();
    let pad: Vec<u8> = c.buf.as_slice()[start..c.buf.pos()].to_vec();
    assert!(
        !contains(&pad, &sentinel_reload()),
        "no table, so nothing clobbers RAX and nothing may reload it"
    );
    assert_eq!(c.local_handler_sites.len(), 0);
    let _ = RAX;
}

/// A `try` that does NOT cover the trapping bci is the same control: the
/// candidate filter is a range test, and an empty candidate list is no
/// site.
#[test]
fn a_range_that_does_not_cover_the_bci_arms_nothing() {
    // Protected [4, 9); the bounds check is at 0.
    let mut c = pad_compiler(vec![(4, 9, 12, "java/lang/RuntimeException")]);
    c.emit_bounds_check(0);
    let start = c.buf.pos();
    c.emit_bounds_check_stubs();
    let pad: Vec<u8> = c.buf.as_slice()[start..c.buf.pos()].to_vec();
    assert!(!contains(&pad, &sentinel_reload()));
    assert_eq!(c.local_handler_sites.len(), 0);
}

/// The armed case: one covering entry ⇒ one site, a `CALL` to the lookup
/// helper, one `CMP EAX, 0 ; JE` rung resolved through `forward_patches`,
/// and the sentinel reload the epilogue needs on the miss edge.
#[test]
fn a_covering_handler_arms_the_bounds_pad_with_a_lookup_and_a_ladder() {
    if !crate::implicit_local_handlers_enabled() {
        // The kill switch is set in this process; the pad is the control
        // shape and the assertions below would be testing it against
        // itself.
        return;
    }
    let mut c = pad_compiler(vec![(0, 9, 12, "java/lang/ArrayIndexOutOfBoundsException")]);
    c.emit_bounds_check(0);
    let start = c.buf.pos();
    let patches_before = c.forward_patches.len();
    c.emit_bounds_check_stubs();
    let pad: Vec<u8> = c.buf.as_slice()[start..c.buf.pos()].to_vec();

    assert_eq!(
        c.local_handler_sites.len(),
        1,
        "one covering entry is one site"
    );
    assert_eq!(
        c.local_handler_sites[0].candidates.len(),
        1,
        "the candidate list is the table filtered by the throwing bci"
    );
    assert_eq!(c.local_handler_sites[0].throw_bci, 0);
    // TEST EAX, EAX ; JS rel32 — the "-1, not ours" edge.
    assert!(
        contains(&pad, &[0x85, 0xC0, 0x0F, 0x88]),
        "the pad must branch out on the helper's -1"
    );
    // CMP EAX, 0 ; JE rel32 — the rung for candidate 0.
    assert!(
        contains(&pad, &[0x83, 0xF8, 0x00, 0x0F, 0x84]),
        "the pad must test the returned candidate index"
    );
    assert!(
        contains(&pad, &sentinel_reload()),
        "the probe's CALL clobbers RAX, so the miss edge owes the epilogue \
         the i64::MIN the throw helper returned"
    );
    assert_eq!(
        c.forward_patches.len(),
        patches_before + 1,
        "the handler jump resolves against pc_to_native like any branch"
    );
    assert_eq!(
        c.forward_patches.last().map(|&(_, target)| target),
        Some(12),
        "and it targets the handler bci"
    );
}

/// The probe must drop the body's register/slot pairing before it emits:
/// a mirror recorded in the body describes a position in the body, and
/// this pad is reached by a branch from somewhere else entirely, so a
/// surviving pairing would let `emit_load_local` elide a reload that is
/// not actually redundant here.
///
/// It does NOT have to end `None`: the probe's own
/// `emit_load_local(heap_local_offset)` records a fresh, correct mirror
/// for the instruction it just emitted. What must not survive is the
/// STALE one.
#[test]
fn the_probe_drops_the_bodys_stale_slot_mirror() {
    if !crate::implicit_local_handlers_enabled() {
        return;
    }
    let mut c = pad_compiler(vec![(0, 9, 12, "")]);
    let stale = (0x18i32, RAX, 0usize);
    c.slot_mirror = Some(stale);
    assert!(c.emit_local_handler_implicit_probe(0));
    assert_ne!(
        c.slot_mirror,
        Some(stale),
        "the body's pairing survived into out-of-line code"
    );
    if let Some((off, _, pos)) = c.slot_mirror {
        assert_eq!(
            off, c.heap_local_offset,
            "the only mirror the probe may leave is its own VM-pointer load"
        );
        assert_ne!(pos, 0, "and it names a position inside the pad");
    }
}

/// Two fallible operations at one bci share ONE site, so they share one
/// monomorphic cache — the property `local_handler_site_by_bci` exists
/// for, restated for the implicit pads that now also create sites.
#[test]
fn two_probes_at_one_bci_share_one_site() {
    if !crate::implicit_local_handlers_enabled() {
        return;
    }
    let mut c = pad_compiler(vec![(0, 9, 12, "")]);
    assert_eq!(c.emit_local_handler_implicit_probe(0), true);
    assert_eq!(c.emit_local_handler_implicit_probe(0), true);
    assert_eq!(c.local_handler_sites.len(), 1);
}

/// A frame with no VM pointer cannot call the lookup helper at all, and
/// the pad must not pass it `[rbp - 0]` — the caller's saved RBP. The one
/// difference from the armed fixture is `needs_heap`.
#[test]
fn a_frame_with_no_heap_slot_refuses_the_probe() {
    if !crate::implicit_local_handlers_enabled() {
        return;
    }
    let mut c = heapless_pad_compiler(vec![(0, 9, 12, "")]);
    assert!(!c.emit_local_handler_implicit_probe(0));
    assert_eq!(
        c.local_handler_sites.len(),
        0,
        "a refused probe must not create a site either"
    );
}

/// nio21 wave 17 L5: an armed bounds pad hands `jit_throw_aioobe` its bci
/// with the snapshot-defer bit (the probe follows the call); the
/// no-table control hands the bare bci. bci 0 makes the immediate
/// `00 00 00 40` exactly.
#[test]
fn nio21w17_l5_an_armed_bounds_pad_defers_its_snapshot_to_the_probe() {
    let deferred_imm = (crate::implicit_null::TRAP_SNAPSHOT_DEFER_BIT as i32).to_le_bytes();
    let mut control = pad_compiler(Vec::new());
    control.emit_bounds_check(0);
    let start = control.buf.pos();
    control.emit_bounds_check_stubs();
    let pad: Vec<u8> = control.buf.as_slice()[start..control.buf.pos()].to_vec();
    assert!(!contains(&pad, &deferred_imm), "nothing follows the setter");
    // Opt-in since the w17 host battery; this half needs the switch set.
    if !crate::implicit_local_handlers_enabled()
    {
        return;
    }
    let mut c = pad_compiler(vec![(0, 9, 12, "java/lang/ArrayIndexOutOfBoundsException")]);
    c.emit_bounds_check(0);
    let start = c.buf.pos();
    c.emit_bounds_check_stubs();
    let pad: Vec<u8> = c.buf.as_slice()[start..c.buf.pos()].to_vec();
    assert!(contains(&pad, &deferred_imm), "the probe follows: defer");
    assert_eq!(
        c.local_handler_sites.len(),
        1,
        "one site, shared by bit and probe"
    );
}

/// nio21 wave 17 L5: an implicit-only arm keeps every dispatched-throwable
/// site (invoke, `athrow`, allocation) on the pre-arm edge.
#[test]
fn nio21w17_l5_an_implicit_only_arm_gives_call_sites_no_local_stub() {
    let mut c = pad_compiler(vec![(0, 9, 12, "")]);
    // bci 5 is an implicit-trap opcode (a `getfield` whose helper raises
    // the NPE signal), bci 3 is not.
    let mut implicit_sites = vec![false; 9];
    implicit_sites[5] = true;
    c.local_handlers_implicit_only = Some(implicit_sites);
    c.record_exception_check_edge(100, 3, false);
    assert!(c.local_handler_stubs.is_empty());
    assert_eq!(c.exception_check_stubs, vec![(100, 3)]);
    c.record_exception_check_edge(200, 5, false);
    assert_eq!(
        c.local_handler_stubs.len(),
        1,
        "the field helper's edge keeps its stub"
    );
    let mut armed = pad_compiler(vec![(0, 9, 12, "")]);
    armed.record_exception_check_edge(100, 3, false);
    assert_eq!(
        armed.local_handler_stubs.len(),
        1,
        "the general arm is unchanged"
    );
}

/// nio21 wave 17 L5: the implicit-only arm is asked only for a method with
/// an implicit-trap opcode inside a protected range, found on instruction
/// starts (an operand byte equal to `iaload` is not one).
#[test]
fn nio21w17_l5_protected_implicit_trap_sites_are_found_on_instruction_starts() {
    use crate::x64::driver::has_protected_implicit_trap_site;
    // aload_0 ; iload_1 ; iaload ; ireturn | handler: astore_2 ; iconst_m1 ; ireturn
    let code = [0x2a, 0x1b, 0x2e, 0xac, 0x4d, 0x02, 0xac];
    let table = vec![(0usize, 3usize, 4usize, "")];
    assert!(has_protected_implicit_trap_site(&code, code.len(), &table));
    // The same range, ending before the `iaload`.
    assert!(!has_protected_implicit_trap_site(
        &code,
        code.len(),
        &[(0, 2, 4, "")]
    ));
    // sipush 0x2e2e ; ireturn: the operand bytes are not opcodes.
    let operand = [0x11, 0x2e, 0x2e, 0xac];
    assert!(!has_protected_implicit_trap_site(
        &operand,
        operand.len(),
        &[(0, 4, 3, "")]
    ));
    assert!(!has_protected_implicit_trap_site(&code, code.len(), &[]));
}

/// A fresh site starts with every implicit kind unresolved — `-1` is the
/// resolved answer "this frame does not catch this kind" and must not be
/// what a never-asked slot reads as.
#[test]
fn a_new_sites_implicit_cache_is_unresolved_not_no_handler() {
    if !crate::implicit_local_handlers_enabled() {
        return;
    }
    let mut c = pad_compiler(vec![(0, 9, 12, "")]);
    assert!(c.emit_local_handler_implicit_probe(0));
    for slot in [
        crate::JitLocalHandlerSite::IMPLICIT_KIND_AIOOBE,
        crate::JitLocalHandlerSite::IMPLICIT_KIND_NPE,
        crate::JitLocalHandlerSite::IMPLICIT_KIND_ARITHMETIC,
    ] {
        assert_eq!(
            c.local_handler_sites[0].implicit_cache[slot]
                .load(std::sync::atomic::Ordering::Relaxed),
            crate::JitLocalHandlerSite::IMPLICIT_UNRESOLVED,
        );
    }
}
