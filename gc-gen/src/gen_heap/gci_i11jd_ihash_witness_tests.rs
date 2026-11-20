use super::gci_i11jd_ihash_verdict as v;

#[test]
fn gci_i11jd_ihash_verdict_names_each_anomaly() {
    // The ordinary answers are silent.
    assert_eq!(v(42, 42, false, false), None);
    assert_eq!(v(42, 0, false, false), None, "re-read raced nothing installed yet");
    assert_eq!(v(7, 0, false, true), None, "a displaced hash from the monitor");
    // The anomalies.
    assert_eq!(v(42, 43, false, false), Some("installed-differs"));
    assert_eq!(v(42, 42, true, false), Some("forwarded"));
    assert_eq!(v(0, 0, true, true), Some("forwarded"), "forwarded wins");
    assert_eq!(v(0, 0, false, true), Some("displaced-none"));
}

/// Source pin: the witness runs after the answer is chosen, on both arms,
/// and only under its switch.
#[test]
fn gci_i11jd_identity_hash_code_calls_the_witness_under_its_switch() {
    let src = concat!(include_str!("../gen_heap/mod.rs"), "\n", include_str!("../gen_heap/young_from_guard.rs"), "\n", include_str!("../gen_heap/geometry_cap_destination.rs"), "\n", include_str!("../gen_heap/publish_young_commit_bits.rs"), "\n", include_str!("../gen_heap/set_resolved_compact_primitive_field.rs"), "\n", include_str!("../gen_heap/set_field_full.rs"), "\n", include_str!("../gen_heap/non_moving_young_cycle.rs"), "\n", include_str!("../gen_heap/collect_garbage_inner_with_pins.rs"), "\n", include_str!("../gen_heap/young_drain_blocked.rs"), "\n", include_str!("../gen_heap/sweep_young_non_moving.rs"), "\n", include_str!("../gen_heap/sweep_old_gen_non_moving.rs"), "\n", include_str!("../gen_heap/mark_young_to_old_refs_deferring.rs"), "\n", include_str!("../gen_heap/cell_corrupt_diag_enabled.rs"), "\n", include_str!("../gen_heap/note_rejected_old_mark_candidate.rs"), "\n", include_str!("../gen_heap/zero_run_verdict.rs")).replace("\r\n", "\n");
    // `rfind`: this test sits above the method, so `find` would hit the
    // needle's own literal.
    let start = src
        .rfind("    pub fn identity_hash_code(&self, obj_ref: ObjectRef) -> i32 {")
        .expect("identity_hash_code");
    let body = &src[start..start + src[start..].find("\n    }\n").expect("end")];
    // gci i12/sx: the switch is the per-heap field latched at construction.
    let gate = body.find("if self.ihash_witness {").expect("gate");
    let call = body
        .find("gci_i11jd_ihash_witness(obj_ref, header, hash, displaced);")
        .expect("call");
    assert!(gate < call);
    assert!(body.trim_end().ends_with("hash"));
}
