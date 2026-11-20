//! zt t8/rv4: the `[GC] zgc-metaspace-backfill:` line reads the VM's
//! own retry-pass count, and only on ZGC.

use super::*;
use crate::config::{GcAlgorithm, VmConfig};

#[test]
fn the_line_counts_the_vms_retry_passes_on_zgc_only() {
    let shared = SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Zgc,
        ..VmConfig::default()
    });
    // zt t11/x8 (x6 item 6): with the `zgc` feature on (the default set),
    // `GcAlgorithm::Zgc` must build a ZGC heap, so the ZGC half below
    // cannot be skipped silently.
    assert!(
        cfg!(not(feature = "zgc"))
            || shared.mem.heap.is_zgc(),
        "GcAlgorithm::Zgc built no ZGC heap"
    );
    if shared.mem.heap.is_zgc() {
        assert_eq!(
            zgc_metaspace_backfill_line(&shared).as_deref(),
            Some("[GC] zgc-metaspace-backfill: retry_passes=0")
        );
        // Positive control: two retry passes show on the line.
        shared.mem.zgc_gc_beans.note_metaspace_backfill_retry_pass();
        shared.mem.zgc_gc_beans.note_metaspace_backfill_retry_pass();
        assert_eq!(
            zgc_metaspace_backfill_line(&shared).as_deref(),
            Some("[GC] zgc-metaspace-backfill: retry_passes=2")
        );
        // Per VM: a second ZGC VM starts at zero.
        let other = SharedVm::new(VmConfig {
            gc_algorithm: GcAlgorithm::Zgc,
            ..VmConfig::default()
        });
        assert_eq!(
            zgc_metaspace_backfill_line(&other).as_deref(),
            Some("[GC] zgc-metaspace-backfill: retry_passes=0")
        );
    }
    for gc in [GcAlgorithm::Generational, GcAlgorithm::G1] {
        let shared = SharedVm::new(VmConfig {
            gc_algorithm: gc,
            ..VmConfig::default()
        });
        assert_eq!(zgc_metaspace_backfill_line(&shared), None);        }    }