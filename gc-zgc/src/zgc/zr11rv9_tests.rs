use super::*;


struct NoMonitors;
impl MonitorCleanup for NoMonitors {
    fn remap_after_gc(&self, _: &cratonvm_types::PointerMap) {}
}

// Mark-pool residual 2: after a failed spawn the next
// `ZGC_MARK_SPAWN_BACKOFF_CYCLES` parallel-mark cycles mark serially
// (and still keep the live set); then spawning is tried again. The kill
// arm never skips.
