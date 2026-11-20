//! gci i12/sx: switches this file used to latch in process `OnceLock`s.
use super::GenerationalHeap;
use cratonvm_types::flags::with_thread_overrides;

/// Source pin: neither reader holds a static.
#[test]
fn gci_i12sx_switch_readers_have_no_process_latch() {
    let src = concat!(
        include_str!("../gen_heap/mod.rs"),
        "\n",
        include_str!("../gen_heap/young_from_guard.rs"),
        "\n",
        include_str!("../gen_heap/geometry_cap_destination.rs"),
        "\n",
        include_str!("../gen_heap/publish_young_commit_bits.rs"),
        "\n",
        include_str!("../gen_heap/set_resolved_compact_primitive_field.rs"),
        "\n",
        include_str!("../gen_heap/set_field_full.rs"),
        "\n",
        include_str!("../gen_heap/non_moving_young_cycle.rs"),
        "\n",
        include_str!("../gen_heap/collect_garbage_inner_with_pins.rs"),
        "\n",
        include_str!("../gen_heap/young_drain_blocked.rs"),
        "\n",
        include_str!("../gen_heap/sweep_young_non_moving.rs"),
        "\n",
        include_str!("../gen_heap/sweep_old_gen_non_moving.rs"),
        "\n",
        include_str!("../gen_heap/mark_young_to_old_refs_deferring.rs"),
        "\n",
        include_str!("../gen_heap/cell_corrupt_diag_enabled.rs"),
        "\n",
        include_str!("../gen_heap/note_rejected_old_mark_candidate.rs"),
        "\n",
        include_str!("../gen_heap/zero_run_verdict.rs")
    )
    .replace("\r\n", "\n");
    for head in [
        // The leading newline skips this array's own literals.
        "\npub fn gci_i11jd_ihash_witness_enabled() -> bool {",
        "\npub fn gen_no_peer_pin_divert() -> bool {",
    ] {
        let start = src.find(head).expect(head);
        let body = &src[start..start + src[start..].find("\n}\n").expect("end")];
        assert!(!body.contains("OnceLock"), "{body}");
        assert!(!body.contains(&["sta", "tic "].concat()), "{body}");
    }
}
