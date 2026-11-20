use super::osr_loud_unresumable_exits;

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

/// Each counted arm asks the switch and answers through
/// `osr_refuse_replay` after counting, before its fallback `return None`.
/// A text pin: neither arm can be reached without a compiled body that
/// leaves through a foreign or missing deopt frame.
#[test]
fn both_counted_arms_refuse_the_replay_after_counting() {
    let body = production_fn("fn try_osr(");
    let flat: String = body.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    for counter in [
        "&OSR_EXIT_FOREIGN_STASH_REPLAYS,",
        "&OSR_EXIT_FRAMELESS_REPLAYS,",
    ] {
        let at = flat.find(counter);
        assert!(at.is_some(), "{counter} is counted in try_osr");
        let rest = &flat[at.unwrap_or(0)..];
        let ask = rest.find("ifosr_loud_unresumable_exits(){returnosr_refuse_replay(");
        let fallback = rest.find("returnNone;");
        assert!(
            matches!((ask, fallback), (Some(a), Some(f)) if a < f),
            "{counter}: the arm refuses the replay before its fallback"
        );
    }
}
