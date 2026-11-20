use super::*;
use crate::config::{GcAlgorithm, VmConfig};

#[test]
fn gcf_f2o_overhead_limit_resolution() {
    let d = GC_OVERHEAD_LIMIT_CYCLES;
    // Nothing given: the historical limit.
    assert_eq!(gc_overhead_limit_cycles_for(None, None, false), d);
    // `-XX:-UseGCOverheadLimit` turns it off; `+` keeps it.
    assert_eq!(gc_overhead_limit_cycles_for(None, Some(false), false), 0);
    assert_eq!(gc_overhead_limit_cycles_for(None, Some(true), false), d);
    // HotSpot 25 default: no limit off Parallel, `+` is a no-op.
    assert_eq!(gc_overhead_limit_cycles_for(None, None, true), 0);
    assert_eq!(gc_overhead_limit_cycles_for(None, Some(true), true), 0);
    // The developer override wins over both.
    assert_eq!(
        gc_overhead_limit_cycles_for(Some("0"), Some(true), false),
        0
    );
    assert_eq!(
        gc_overhead_limit_cycles_for(Some(" 3 "), Some(false), true),
        3
    );
    assert_eq!(
        gc_overhead_limit_cycles_for(Some("junk"), Some(false), false),
        d
    );
}
