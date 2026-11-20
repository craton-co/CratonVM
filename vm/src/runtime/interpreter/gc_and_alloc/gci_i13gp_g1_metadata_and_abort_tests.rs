//! gci i13/gp (`gci-i12ge-g1-event-model-review-findings-RETIRED-UNFIXED-20261018` items 3
//! and 5): the metadata concurrent start and the full collection's abort
//! of an open G1 cycle, both opt-in.
use super::*;
use crate::config::{GcAlgorithm, VmConfig};
use crate::threading::jvm_thread::{JvmThread, ThreadId};
use crate::vm::SharedVm;
use cratonvm_types::flags::with_thread_overrides;
use std::sync::atomic::Ordering;


fn vm(gc_algorithm: GcAlgorithm) -> SharedVm {
    SharedVm::new(VmConfig {
        gc_algorithm,
        ..VmConfig::default()
    })
}

fn source() -> String {
    super::GC_AND_ALLOC_SRC.replace("\r\n", "\n")
}

fn body(src: &str, sig: &str) -> String {
    let start = src.find(sig).unwrap_or_else(|| panic!("`{sig}` not found"));
    let rest = &src[start..];
    let stop = rest
        .find("\n\u{7d}\n")
        .unwrap_or_else(|| panic!("end of `{sig}` not found"));
    rest[..stop].to_string()
}


/// The shape: the metadata door tries the concurrent start first and keeps
/// its full collection; the start declines while a cycle marks, reads the
/// switch, runs its pause on the metadata door and opens the cycle after
/// it; the full collection marks the open cycle aborted before its pause.
#[test]
fn gci_i13gp_the_doors_shape() {
    let src = source();
    let meta = body(&src, "pub fn force_gc_for_class_metadata(");
    let cs = meta
        .find("g1_metadata_concurrent_start(shared, thread)")
        .expect("the concurrent start first");
    let full = meta
        .find("force_full_collection(shared, thread, GcDoor::MetadataThreshold)")
        .expect("the full collection kept");
    assert!(cs < full);
    let start = body(&src, "fn g1_metadata_concurrent_start(");
    let active = start.find("g1_is_marking_active()").expect("declines while marking");
    let switch = start
        .find("g1_metadata_concurrent_start_enabled()")
        .expect("reads its switch");
    let pause = start
        .find("run_collection_pause(shared, thread, GcDoor::MetadataThreshold)")
        .expect("the young pause on the metadata door");
    let mark = start
        .find("g1_concurrent_mark_cycle(shared, thread)")
        .expect("opens the cycle");
    assert!(active < switch && switch < pause && pause < mark);
    let f = body(&src, "fn force_full_collection(");
    let abort = f
        .find("g1_full_gc_abort_open_cycle(shared, door)")
        .expect("the abort mark");
    let first = f
        .find("for _attempt in 0..FORCE_GC_ATTEMPTS")
        .expect("attempt loop");
    assert!(abort < first);
}
