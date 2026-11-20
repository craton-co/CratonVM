/// Tier proposal 29: under background compilation `try_osr` queues an OSR
/// task for the worker instead of building the optimizing body on the
/// mutator, and that arm is asked before the build. A text pin, because
/// `try_osr` needs a live frame, a loaded class and a backend.
#[test]
fn try_osr_leaves_the_optimizing_build_to_the_worker_under_bg_compile() {
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
    let src = whole.as_str();
    let body = &src[src
        .find("\npub(in crate::runtime::interpreter) fn try_osr(")
        .expect("try_osr exists")..];
    let body = &body[..body.find("\n\u{7d}\n").map_or(body.len(), |e| e + 3)];
    // Whitespace stripped, so the pin survives any wrapping `cargo fmt`
    // chooses.
    let flat: String = body.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    let defer = flat
        .find(
            "}elseifcrate::runtime::env_cache::bg_compile()\
             &&!osr_optimizing_cache_disabled()",
        )
        .expect("the background arm exists");
    let queue = flat[defer..]
        .find(".request_osr(")
        .map(|i| i + defer)
        .expect("the background arm queues an OSR task");
    let build = flat
        .find("build_osr_optimizing_artifact(")
        .expect("the mutator build stays for the kill switches");
    assert!(
        defer < build && queue < build,
        "the background arm must be asked before the mutator build"
    );
}
