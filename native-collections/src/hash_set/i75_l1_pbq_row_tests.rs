use super::*;
use cratonvm_types::compat::CompatibilityMode;
use cratonvm_types::{HotSpotAlignment as Row, HotSpotAlignments};

const PBQ: &str = "java/util/concurrent/PriorityBlockingQueue";

fn rows(real_jdk: bool, on: bool) -> usize {
    let mut r = NativeMethodRegistry::new();
    r.set_compatibility_mode(CompatibilityMode::Compatible);
    r.set_hotspot_alignments(HotSpotAlignments::NONE.with(Row::JdkPriorityBlockingQueue, on));
    r.set_real_jdk(real_jdk);
    register_priority_blocking_queue_natives(&mut r);
    r.dump_registrations()
        .into_iter()
        .filter(|(c, _, _, _)| *c == PBQ)
        .count()
}

#[test]
fn the_queue_stand_ins_follow_the_priority_blocking_queue_row() {
    for (m, d) in [
        ("<init>", "()V"),
        ("offer", "(Ljava/lang/Object;)Z"),
        ("take", "()Ljava/lang/Object;"),
        ("size", "()I"),
    ] {
        if cratonvm_native_api::unretire::is_excluded(PBQ, m, d) {
            return; // a retired row was un-retired in this environment
        }
    }
    assert_eq!(rows(true, false), 9);
    assert_eq!(rows(true, true), 0);
    // The synthetic library: the row needs a real JDK.
    assert_eq!(rows(false, true), 9);
    assert_eq!(rows(false, false), 9);
}
