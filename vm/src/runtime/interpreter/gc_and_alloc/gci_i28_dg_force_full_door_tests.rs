use super::*;

#[test]
fn gci_i28dg_only_user_requested_doors_may_run_a_concurrent_full_cycle() {
    assert!(!g1_door_forces_stw_full_cycle(GcDoor::SystemGc));
    assert!(!g1_door_forces_stw_full_cycle(GcDoor::DiagnosticCommand));
    for door in [
        GcDoor::JvmtiForceGc,
        GcDoor::HeapInspection,
        GcDoor::HeapDump,
        GcDoor::MetadataThreshold,
    ] {
        assert!(g1_door_forces_stw_full_cycle(door), "{door:?}");
    }
}

#[test]
fn gci_i28dg_the_full_collection_picks_the_cycle_by_its_door() {
    let src = super::GC_AND_ALLOC_SRC.replace("\r\n", "\n");
    let f = &src[src
        .find("fn force_full_collection(")
        .expect("force_full_collection")..];
    let f = &f[..f.find("\n\u{7d}\n").expect("its end")];
    let gate = f
        .find("if g1_door_forces_stw_full_cycle(door) \u{7b}")
        .expect("the door gate");
    let stw = f
        .find("g1_force_full_cycle_stw(shared, thread);")
        .expect("the STW cycle");
    let plain = f
        .find("g1_force_full_cycle(shared, thread);")
        .expect("the policy's cycle");
    assert!(gate < stw && stw < plain);
    let w = &src[src
        .find("\npub(crate) fn g1_force_full_cycle_stw(")
        .expect("the STW door")..];
    assert!(w.contains("g1_force_full_cycle_with(shared, thread, None, true)"));
    let c = &src[src.find("fn g1_force_full_cycle_with(").expect("the loop")..];
    assert!(c.contains(
        "if g1_force_full_cycle_is_stw(shared.config.explicit_gc) || door_forces_stw \u{7b}"
    ));
}
