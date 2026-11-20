use super::{maybe_drain_jit_entry_counters_at_osr_entry, OSR_ENTRY_DRAIN_STRIDE};
use crate::config::VmConfig;
use crate::vm::SharedVm;
use std::sync::atomic::Ordering;

fn production_fn(sig: &str) -> String {
    let whole = concat!(
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
    let src = &whole;
    let start = src.find(sig);
    assert!(start.is_some(), "`{sig}` not found");
    let start = start.unwrap_or(0);
    let body = &src[start..];
    body[..body.find("\n\u{7d}\n").map_or(body.len(), |e| e + 3)].to_string()
}

/// P3: the first strided OSR entry drains and stamps the drain clock; the
/// next `OSR_ENTRY_DRAIN_STRIDE - 1` entries only count.
#[test]
fn the_osr_entry_drain_trigger_is_strided_and_stamps_the_clock() {
    if !crate::jit::entry_counter::entry_counter_enabled()
        || !crate::jit::entry_counter::callee_promotion_enabled()
    {
        return; // a kill switch is set in this environment
    }
    let shared = SharedVm::new(VmConfig::default());
    let bridge = &shared.jit.bridge;
    assert_eq!(
        bridge.last_entry_counter_drain_ms.load(Ordering::Relaxed),
        0
    );
    maybe_drain_jit_entry_counters_at_osr_entry(&shared);
    let first = bridge.last_entry_counter_drain_ms.load(Ordering::Relaxed);
    assert_ne!(first, 0, "the first strided OSR entry drains");
    for _ in 1..OSR_ENTRY_DRAIN_STRIDE {
        maybe_drain_jit_entry_counters_at_osr_entry(&shared);
    }
    assert_eq!(
        bridge.last_entry_counter_drain_ms.load(Ordering::Relaxed),
        first,
        "entries between two strides read no clock and drain nothing"
    );
    assert_eq!(
        bridge.osr_entry_drain_ticks.load(Ordering::Relaxed),
        OSR_ENTRY_DRAIN_STRIDE
    );
}

/// SBR4 osr: the OSR door registers `System.arraycopy` as an intrinsic and
/// MUST register the site's `JitInvokeInfo` with it, as the method-entry
/// scan does. Without the info a de-specialised site (four `BoundsCheck`
/// traps, which a reference-element array takes every time) lowered to the
/// self-recursive call arm: `TimSort.binarySort`'s OSR body called itself
/// with the arraycopy operands (wrong sorts, NPE, SIGSEGV). A text pin: the
/// door needs a loaded class and a frame to run.
#[test]
fn the_osr_door_registers_the_arraycopy_dispatch_info_beside_the_intrinsic() {
    let body = production_fn("fn compile_osr_body(");
    let flat: String = body.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    // `assert!` + `unwrap_or`, not `.expect(`: `production_fn`'s `"\n\u{7d}\n"`
    // literal unbalances the panic gate's brace count
    // (`tests.rs` `scan_production_section`), so the lines below are
    // scanned as production code.
    let entry = flat.find("JitIntrinsic::ArraycopyPrimitive.as_entry()");
    assert!(
        entry.is_some(),
        "the OSR door must name the arraycopy intrinsic"
    );
    let entry = entry.unwrap_or(0);
    let info = flat[entry..].find("push_osr_invoke_info(");
    assert!(
        info.is_some(),
        "the arraycopy intrinsic needs its invoke info at the OSR door"
    );
    let row = flat[entry..].find("direct_calls2.push(");
    assert!(
        row.is_some(),
        "the intrinsic row is registered after the info"
    );
    assert!(info < row, "the info is pushed before the direct-call row");
}

/// P1: each of the three doors the manager does not schedule reports its
/// publish, and the callee compiler reports only a body `put` kept.
#[test]
fn the_three_outside_doors_report_their_publish_to_the_tiered_manager() {
    let osr = production_fn("fn try_osr(");
    assert!(osr.contains("maybe_drain_jit_entry_counters_at_osr_entry(shared);"));
    let sync = production_fn("fn sync_direct_compile_unpublished_callee(");
    assert!(sync.contains("note_callee_publish_to_tiered_manager("));
    let flat = |s: String| -> String { s.chars().filter(|c| !c.is_ascii_whitespace()).collect() };
    let optimizing = flat(production_fn("fn compile_optimizing_artifact("));
    let put = optimizing.find(".filter(|body|body.entry_ptr()asusize==entry)");
    assert!(
        put.is_some(),
        "the callee compiler reads its own body back after `put`"
    );
    let put = put.unwrap_or(0);
    assert!(
        optimizing[put..].contains("note_callee_publish_to_tiered_manager("),
        "and reports it"
    );
    let slow = production_fn("fn try_jit_compile_callee_slow(");
    assert!(
        slow.contains("note_callee_publish_to_tiered_manager("),
        "the eager callee chain reports its bound body"
    );
}
