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
    let body = &src[start.unwrap_or(0)..];
    // `\u{7d}` is `}`: spelled so this line's braces balance for the
    // panic gates' line-by-line brace count (`scan_production_section`).
    body[..body.find("\n\u{7d}\n").map_or(body.len(), |e| e + 3)].to_string()
}

fn flat(s: &str) -> String {
    s.chars().filter(|c| !c.is_ascii_whitespace()).collect()
}

#[test]
fn a_counted_value_equivalent_exit_commits_the_entry() {
    let transfer = flat(&production_fn("fn osr_optimizing_transfer_guard_exit("));
    assert!(
        transfer.contains(
            "*counted_out=note_osr_optimizing_value_equivalent_exit(shared,compiled,point_addr);"
        ),
        "the transfer reports what the memo's bound counted"
    );
    let note = flat(&production_fn(
        "fn note_osr_optimizing_value_equivalent_exit(",
    ));
    assert!(note.contains("if!value_equivalent{returnfalse;}"));
    let door = flat(&production_fn("fn try_osr("));
    assert!(
        door.contains("||expected_throw_exit||counted_value_equivalent;"),
        "a counted exit is committed, not charged against the frame"
    );
}
