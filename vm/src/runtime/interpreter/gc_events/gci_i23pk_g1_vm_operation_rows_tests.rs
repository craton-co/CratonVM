//! gci i23/pk (`gci-i20ob-g1-remark-and-cleanup-write-no-vm-operation-rows-FIXED-20261016`):
//! G1's Remark and Cleanup pause rows also write HotSpot's VM-operation
//! rows (`G1PauseRemark`, `G1PauseCleanup`) to the backend; nothing else.
use super::*;

#[test]
fn gci_i23pk_only_g1_remark_and_cleanup_rows_are_vm_operations() {
    assert_eq!(
        g1_cycle_pause_vm_operation(true, "Pause Remark"),
        Some("G1PauseRemark")
    );
    assert_eq!(
        g1_cycle_pause_vm_operation(true, "Pause Cleanup"),
        Some("G1PauseCleanup")
    );
    // G1's initial mark has no row of its own; its name is the young
    // pause's, which `gc_cycle` writes as a collection.
    assert_eq!(g1_cycle_pause_vm_operation(true, "GC Pause"), None);
    // The Generational cycle has no HotSpot VM operation.
    for name in ["Pause Remark", "Pause Cleanup", "Pause Init Mark"] {
        assert_eq!(g1_cycle_pause_vm_operation(false, name), None, "{name}");
    }
    // Every row the remark splitter makes maps on G1.
    for (name, _, _) in remark_cleanup_rows(1_000, 900, Some(600)) {
        assert!(g1_cycle_pause_vm_operation(true, name).is_some(), "{name}");
    }
}

/// The writer is called on the backend half only, per pause row, with
/// the row's G1 bit; the only `GenCycleJfrRow` literal sets that bit from
/// the cycle's backend.
#[test]
fn gci_i23pk_the_backend_half_writes_the_vm_operation() {
    let src = include_str!("../gc_events.rs").replace("\r\n", "\n");
    let at = src
        .find("\nfn emit_gen_cycle_jfr_row(")
        .expect("the emitter");
    let body = &src[at..];
    let body = &body[..body.find("\n\u{7d}\n").expect("its end")];
    let backend = body.find("if let Some(vm) = row.backend_vm \u{7b}").expect("backend half");
    let call = body
        .find("if let Some(op) = g1_cycle_pause_vm_operation(row.g1, name) \u{7b}")
        .expect("the call");
    let writer = body
        .find("jfr_vm_more::vm_operation_pause(")
        .expect("the writer");
    assert!(backend < call && call < writer);
    let at = src.find("\nfn gen_cycle_jfr_row(").expect("the builder");
    let builder = &src[at..];
    let builder = &builder[..builder.find("\n\u{7d}\n").expect("its end")];
    assert!(builder.contains("g1: !generational,"));
}
