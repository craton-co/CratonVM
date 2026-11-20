//! gcf f3/o: the `[GC] oome-holder:` line and the fifth grace door.
use super::*;
use crate::config::{GcAlgorithm, VmConfig};
use crate::threading::jvm_thread::{JvmThread, ThreadId};
use cratonvm_gc::gen_heap::oome_ladder as l;

#[test]
fn gcf_f3o_oome_holder_lines_are_one_per_marker_or_none() {
    assert_eq!(
        oome_holder_lines(3, &[]),
        vec!["[GC] oome-holder: site=native-after-oome-major thread=3 none".to_string()]
    );
    let s = vec!["marker#1 0x10 long[] reach_bytes=8 | holder#1 root[2]=0x20".to_string()];
    assert_eq!(
        oome_holder_lines(3, &s),
        vec![format!(
            "[GC] oome-holder: site=native-after-oome-major thread=3 {}",
            s[0]
        )]
    );
}

/// A graced native retry is counted once, by its own thread's next
/// successful allocation; another thread's allocation leaves it pending.
#[test]
fn gcf_f3o_a_graced_native_retry_is_counted_by_its_thread() {
    let shared = SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Generational,
        ..VmConfig::default()
    });
    let ok = |s: &SharedVm| s.mem.heap.gen_oome_ladder_census().map(|c| c[l::LATCHED_GRACE_OK]).unwrap_or(0);
    let me = JvmThread::new(ThreadId(1), "f3o-me");
    let other = JvmThread::new(ThreadId(2), "f3o-other");
    note_native_alloc_succeeded(&shared, &me);
    assert_eq!(ok(&shared), 0, "nothing pending");
    note_graced_native_retry(&shared, &me);
    note_native_alloc_succeeded(&shared, &other);
    assert_eq!(ok(&shared), 0, "another thread's allocation");
    note_native_alloc_succeeded(&shared, &me);
    assert_eq!(ok(&shared), 1);
    note_native_alloc_succeeded(&shared, &me);
    assert_eq!(ok(&shared), 1, "counted once");
    // gcf f4/o: a graced retry that failed (its OOME was raised) is not
    // counted by a later success.
    note_graced_native_retry(&shared, &me);
    note_heap_oome_raised(&shared, &me, "Java heap space");
    note_native_alloc_succeeded(&shared, &me);
    assert_eq!(ok(&shared), 1, "a failed graced retry is not a success");
}

/// gcf f4/o: the per-door census is per VM and every door has its keys.
#[test]
fn gcf_f4o_oome_ladder_doors_line_counts_per_door() {
    let shared = SharedVm::new(VmConfig::default());
    note_ladder_door_entered(&shared, LadderDoor::Jit);
    note_ladder_door_entered(&shared, LadderDoor::Jit);
    note_ladder_door_allocated(&shared, LadderDoor::Jit);
    note_ladder_door_entered(&shared, LadderDoor::NativeFactory);
    note_ladder_door_allocated(&shared, LadderDoor::NativeFactory);
    assert_eq!(
        oome_ladder_doors_line(&shared),
        "[GC] oome_ladder_doors: interp_enter=0 interp_ok=0 native_enter=0 native_ok=0 \
         factory_enter=1 factory_retry=1 jit_enter=2 jit_ok=1 jni_enter=0 jni_ok=0"
    );
    let other = SharedVm::new(VmConfig::default());
    assert!(oome_ladder_doors_line(&other).contains("jit_enter=0"));
}

