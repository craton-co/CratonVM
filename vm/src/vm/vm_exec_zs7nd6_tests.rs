// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! zs s7/nd6 tests (ZGC round zs, wave s7, lane nd6), a child module of
//! `vm_exec` (`#[path]`).

use super::*;
use crate::config::{GcAlgorithm, VmConfig};

fn zgc_shared() -> SharedVm {
    SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Zgc,
        max_heap_size: 64 << 20,
        ..VmConfig::default()
    })
}

/// §8 of `zgc-zs2nd2-native-door-residuals-SUPERSEDED-20261019.md`: an embedder that
/// never raises the init level to 4 latches its boot finished; from then on a
/// refused start-up object is not VM initialisation's failure. Before the
/// latch the predicate read the level alone, so this VM (level < 4 for good)
/// answered "init error" forever.
#[test]
fn zs7nd6_an_embedders_boot_latch_ends_the_init_error_window() {
    let shared = zgc_shared();
    assert!(shared.get_init_level() < 4, "fixture: never raised to 4");
    assert!(!shared.boot_finished());
    assert!(
        super::super::vm_object::boot_oome_is_init_error(&shared),
        "still booting: a refused start-up object ends VM initialisation"
    );
    shared.mark_boot_finished();
    assert!(shared.boot_finished());
    assert!(
        !super::super::vm_object::boot_oome_is_init_error(&shared),
        "the embedder finished its boot: no init error"
    );
    // Per VM: a second VM in the same process is still booting.
    let other = zgc_shared();
    assert!(super::super::vm_object::boot_oome_is_init_error(&other));
}

/// Kill control (`CRATONVM_ZGC_EMBED_BOOT_LATCH=0`): the level alone, the s2
/// rule -- the latched VM still answers "init error" (the "before").
#[test]
fn zs7nd6_the_kill_switch_reads_the_level_alone() {
    let _off = cratonvm_types::flags::override_thread(
        cratonvm_types::flags::VmFlags::from_env_with_edits(&[(
            "CRATONVM_ZGC_EMBED_BOOT_LATCH",
            Some("0"),
        )]),
    );
    let shared = zgc_shared();
    shared.mark_boot_finished();
    assert!(!shared.boot_finished());
    assert!(super::super::vm_object::boot_oome_is_init_error(&shared));
}
