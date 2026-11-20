use super::osr_exception_exit_bcis_uncovered;
use cratonvm_reader::attribute::ExceptionTableEntry;

fn row(start_pc: u16, end_pc: u16) -> ExceptionTableEntry {
    ExceptionTableEntry {
        start_pc,
        end_pc,
        handler_pc: end_pc,
        catch_type: 0,
    }
}

/// A publishing-nothing exit inside a row would propagate past a handler
/// that covers it, so it refuses the body; one outside every row admits
/// it. `[start_pc, end_pc)` is half-open, as in the interpreter.
#[test]
fn a_covered_exit_refuses_and_an_uncovered_one_admits() {
    let table = [row(4, 14)];
    assert!(
        osr_exception_exit_bcis_uncovered(Some(&[2u32, 14, 20][..]), &table),
        "outside [4, 14): end_pc is exclusive"
    );
    assert!(
        !osr_exception_exit_bcis_uncovered(Some(&[2u32, 4][..]), &table),
        "start_pc is inclusive"
    );
    assert!(!osr_exception_exit_bcis_uncovered(
        Some(&[13u32][..]),
        &table
    ));
    assert!(
        osr_exception_exit_bcis_uncovered(Some(&[][..]), &table),
        "no publishing-nothing exit at all"
    );
    assert!(
        !osr_exception_exit_bcis_uncovered(None, &table),
        "a body with a guard exit is never admitted here"
    );
    assert!(
        osr_exception_exit_bcis_uncovered(Some(&[5u32][..]), &[]),
        "no table: nothing can catch"
    );
}

/// Round 11 wave 11 (`r11w10-orch-osr-exc-exits-loses-handler-updated-locals`):
/// the door's handler-reach test on `throwLoop`'s shape. The `catch` that
/// falls back into the loop reaches the header (4), so an exits-admitted
/// body is refused there; a `catch` that rethrows (the javac
/// `synchronized` / `finally` shape) does not, so that population keeps
/// the route. The frame's padded buffer is what the door passes.
#[test]
fn a_header_a_catch_falls_back_to_is_closed_and_a_rethrowing_one_is_not() {
    use super::osr_handler_reachable_pcs;
    use cratonvm_jit::deopt::pc_reachable_from_handlers;
    // 0 iconst_0; 1 istore_1; 2 iconst_0; 3 istore_2; 4 iload_2 <- header
    // 5 iload_0; 6 if_icmpge 27; 9 iload_2 <- try [9, 14)
    // 10 invokestatic #1; 13 pop; 14 goto 21; 17 astore_3 <- handler
    // 18 iinc 1, 1; 21 iinc 2, 1; 24 goto 4; 27 iload_1; 28 ireturn; pad
    let continues: [u8; 31] = [
        0x03, 0x3c, 0x03, 0x3d, 0x1c, 0x1a, 0xa2, 0x00, 0x15, 0x1c, 0xb8, 0x00, 0x01, 0x57,
        0xa7, 0x00, 0x07, 0x4e, 0x84, 0x01, 0x01, 0x84, 0x02, 0x01, 0xa7, 0xff, 0xec, 0x1b,
        0xac, 0x00, 0x00,
    ];
    // The same, the handler being `astore_3; aload_3; athrow; nop`.
    let mut rethrows = continues;
    rethrows[18] = 0x2d;
    rethrows[19] = 0xbf;
    rethrows[20] = 0x00;
    let table = [ExceptionTableEntry {
        start_pc: 9,
        end_pc: 14,
        handler_pc: 17,
        catch_type: 0,
    }];
    let reach = osr_handler_reachable_pcs(&continues, &table);
    assert!(reach.is_some());
    assert!(pc_reachable_from_handlers(reach.as_deref(), 4));
    let reach = osr_handler_reachable_pcs(&rethrows, &table);
    assert!(reach.is_some());
    assert!(!pc_reachable_from_handlers(reach.as_deref(), 4));
    assert_eq!(
        osr_handler_reachable_pcs(&continues, &[]),
        Some(Vec::new()),
        "no table: no handler reaches anything"
    );
}

/// `r11w11-tier-osr-optimizing-no-stub-body-kept-for-other-headers-patch`:
/// a body the build's own pc cannot enter (no stub there, or a header a
/// handler reaches) is still kept under the other headers it has a stub
/// for, and each of those is asked the handler test too. A text pin,
/// because the build needs a backend and a loaded class.
#[test]
fn a_body_unenterable_at_its_own_pc_is_still_kept_for_its_other_headers() {
    let src = concat!(include_str!("../jit_bridge/mod.rs"), "\n", include_str!("../jit_bridge/jit_verdicts.rs"), "\n", include_str!("../jit_bridge/osr_continuation.rs"), "\n", include_str!("../jit_bridge/compile_osr_body.rs"), "\n", include_str!("../jit_bridge/osr_exception_exit.rs"), "\n", include_str!("../jit_bridge/try_osr.rs"), "\n", include_str!("../jit_bridge/native_shadow_arms.rs"), "\n", include_str!("../jit_bridge/compile_optimizing_artifact.rs"), "\n", include_str!("../jit_bridge/never_run_static_exit_pcs.rs"), "\n", include_str!("../jit_bridge/try_jit_compile_callee_slow.rs"), "\n", include_str!("../jit_bridge/byte_array_field_declarer_admitted.rs"), "\n", include_str!("../jit_bridge/folded_handle_member_row.rs"), "\n", include_str!("../jit_bridge/callee_never_taken_rows.rs"), "\n", include_str!("../jit_bridge/mint_ldc_slots_at_offer.rs")).replace("\r\n", "\n");
    // A real newline before `fn`: only the definition matches, never the
    // escaped `\n` of this test's own literal.
    let body = &src[src
        .find("\npub(in crate::runtime::interpreter::jit_bridge) fn build_osr_optimizing_artifact(")
        .expect("the build exists")..];
    let body = &body[..body.find("\n\u{7d}\n").map_or(body.len(), |e| e + 3)];
    let flat: String = body.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    let refusal = flat
        .find("note_osr_optimizing_refusal(shared,key,stamps.jit_redefine_epoch);")
        .expect("the per-pc refusal");
    let fan_out = flat
        .find("for&(bci,_,_)inartifact.ir_osr_entries.iter()")
        .expect("the fan-out over the body's other headers");
    assert!(refusal < fan_out);
    assert!(
        !flat[refusal..fan_out].contains("returnNone;"),
        "the per-pc refusal must not drop the body before the fan-out"
    );
    assert!(
        flat[fan_out..].contains("ifclosed_by_a_handler(pc)"),
        "every other header is asked the handler test"
    );
    assert!(flat.contains("enterable_here.then_some((artifact,callee_pins))"));
}

/// `r11-orch-osr-optimizing-body-hides-its-direct-callee-frame-FIXED-20260925`:
/// the optimizing OSR body bypasses `JitCache`, so the build itself must
/// give it the compile-id bind and code-range registration `put_osr` gives
/// every published body, BEFORE it is remembered (and so before anything
/// can enter it). Without them its direct callee's frame is misnamed and
/// its own frame is invisible to every frame walker, the GC's included. A
/// text pin, because the build needs a backend and a loaded class; the
/// behaviour itself is pinned in `conservative_roots`
/// (`an_unpublished_osr_body_names_its_direct_callee_once_registered`).
#[test]
fn the_optimizing_osr_body_is_registered_before_it_can_be_entered() {
    let src = concat!(
        include_str!("../jit_bridge/mod.rs"),
        "\n",
        include_str!("../jit_bridge/jit_verdicts.rs"),
        "\n",
        include_str!("../jit_bridge/osr_continuation.rs"),
        "\n",
        include_str!("../jit_bridge/compile_osr_body.rs"),
        "\n",
        include_str!("../jit_bridge/osr_exception_exit.rs"),
        "\n",
        include_str!("../jit_bridge/try_osr.rs"),
        "\n",
        include_str!("../jit_bridge/native_shadow_arms.rs"),
        "\n",
        include_str!("../jit_bridge/compile_optimizing_artifact.rs"),
        "\n",
        include_str!("../jit_bridge/never_run_static_exit_pcs.rs"),
        "\n",
        include_str!("../jit_bridge/try_jit_compile_callee_slow.rs"),
        "\n",
        include_str!("../jit_bridge/byte_array_field_declarer_admitted.rs"),
        "\n",
        include_str!("../jit_bridge/folded_handle_member_row.rs"),
        "\n",
        include_str!("../jit_bridge/callee_never_taken_rows.rs"),
        "\n",
        include_str!("../jit_bridge/mint_ldc_slots_at_offer.rs")
    )
    .replace("\r\n", "\n");
    // No `.expect` in this test: the production-panic gates' brace count
    // reads this module's tail as production code.
    let start = src.find(
        "\npub(in crate::runtime::interpreter::jit_bridge) fn build_osr_optimizing_artifact(",
    );
    assert!(start.is_some(), "the build exists");
    let body = &src[start.unwrap_or(0)..];
    // The function's closing line, spelled without a brace literal: the
    // production-panic gates (`hot_files_have_no_production_panics`) find
    // this module's end by counting braces, and an unmatched one inside a
    // string ended the module early for them.
    let close = format!("\n{}\n", char::from(0x7d_u8));
    let body = &body[..body.find(close.as_str()).map_or(body.len(), |e| e + 3)];
    let flat: String = body.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    let arc = flat.find("letartifact=Arc::new(cm);");
    let register = flat.find("cratonvm_jit::register_unpublished_body(&artifact);");
    let remember = flat.find("remember_osr_optimizing_artifact(");
    assert!(arc.is_some(), "the artifact is shared here");
    assert!(register.is_some(), "the build registers the body it keeps");
    assert!(remember.is_some(), "the build remembers the body");
    assert!(arc < register, "registered once it has its final address");
    assert!(
        register < remember,
        "registered before any door can serve it"
    );
}
