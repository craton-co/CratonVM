use super::fall_through_resolves_more_calls;

fn body(inlined_sites: u32, direct: usize) -> cratonvm_jit::CompiledMethod {
    let mut buf = cratonvm_jit::ExecutableBuffer::new(64).unwrap();
    buf.emit(&[0xC3]); // ret -- metadata-only fixture, never executed
    let mut cm = cratonvm_jit::CompiledMethod::new(buf);
    cm.inline_tally.inlined_sites = inlined_sites;
    cm._direct_callee_entries = (0..direct).map(|i| 0x2000 + i).collect();
    cm
}

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

/// A site spliced in one compile and direct-bound in the next trades one
/// count for the other. Wave 1's either-count rule kept both directions
/// (a replace on every refused supersede); the sum keeps neither, and still
/// keeps a body that resolves strictly more.
#[test]
fn a_splice_bind_trade_is_not_kept_in_either_direction() {
    assert!(!fall_through_resolves_more_calls(&body(1, 0), &body(0, 1)));
    assert!(!fall_through_resolves_more_calls(&body(0, 1), &body(1, 0)));
    assert!(!fall_through_resolves_more_calls(&body(2, 0), &body(0, 2)));
    assert!(fall_through_resolves_more_calls(&body(1, 1), &body(0, 1)));
    assert!(fall_through_resolves_more_calls(&body(0, 3), &body(2, 0)));
}

/// The published-body probe remains lookup-only. The enclosing binder may
/// additionally use the bounded static chain, and both inline resolvers
/// receive that same binder under the switch.
#[test]
fn the_in_place_callee_compile_pins_published_and_bounded_static_binds() {
    let lookup = flat(&production_fn("fn published_direct_callee_bind("));
    for compiles in [
        "try_compile",
        "try_jit_compile_callee",
        "compile_optimizing_artifact",
        "sync_direct_target",
        "eager_callee_chain",
    ] {
        assert!(
            !lookup.contains(compiles),
            "the lookup must not reach `{compiles}`"
        );
    }
    assert!(lookup.contains("jit_cache.get(&declaring_name,"));
    let door = flat(&production_fn("fn compile_optimizing_artifact("));
    assert!(door.contains("published_direct_callee_bind(shared,&c_cp.substituted_owners,"));
    assert!(
        door.contains("compile_unpublished_static_callee_bind(shared,&c_cp.substituted_owners,")
    );
    // `\u{7b}` is `{`, spelled for the panic gates' brace count.
    assert!(door.contains("callee_compiler:ifc_lookup_on\u{7b}Some(&c_direct_lookup)"));
    assert!(door.contains("inline_resolver:ifc_lookup_on\u{7b}Some(&c_inline_resolver)"));
    let spliced_bind = "Some(&c_direct_lookup_inasInlineDirectBind<'_>),";
    assert!(door.contains(&format!("{spliced_bind}Some(&c_cp.substituted_owners),")));
    // The bound bodies stay pinned until the callee's `put`.
    let pins = door.find("c_bind_pins.borrow_mut().push(body);");
    let put = door.find(".filter(|body|body.entry_ptr()asusize==entry)");
    assert!(pins.is_some() && put.is_some() && pins < put);
    // R12-4 on this door: a single-pass in-place body never replaces an
    // optimizing one published while it compiled.
    let keep = door.find(
        "compiled.keep_live_optimizing_body=crate::jit::tiered::keep_live_optimizing_body_enabled();",
    );
    assert!(keep.is_some() && keep < put);
}

/// CR15-4: both compiles on the optimizing door take their own verdict
/// right after the compile, on the declined path too (before the `?`),
/// and hand the enclosing one back after it.
#[test]
fn the_optimizing_door_takes_its_own_verdict_before_the_decline() {
    let door = flat(&production_fn("fn compile_optimizing_artifact("));
    let callee = door.find("letc_outer_verdict=");
    let callee_take = door.find("let_=cratonvm_jit::ir_evidence::take_last_verdict();");
    let callee_back = door.find("drop(c_outer_verdict);letmutcompiled=compiled?;");
    assert!(callee.is_some() && callee < callee_take && callee_take < callee_back);
    let outer = door.find("letouter_verdict=own_verdict_on.then(NestedVerdictHandOff::");
    let outer_take = door.find("letverdict=ifown_verdict_on\u{7b}cratonvm_jit::ir_evidence::");
    let outer_back = door.find("drop(outer_verdict);letcompiled=compiled?;");
    assert!(outer.is_some() && outer < outer_take && outer_take < outer_back);
    let osr = flat(&production_fn("fn build_osr_optimizing_artifact("));
    assert!(osr.contains("own_verdict.or_else(cratonvm_jit::ir_evidence::take_last_verdict)"));
}
