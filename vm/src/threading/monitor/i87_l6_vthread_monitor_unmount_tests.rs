//! Interpreter round i4 wave 87, lane L6: the unmounted-entrant queue and
//! the release hand-off of `CRATONVM_VTHREAD_MONITOR_UNMOUNT`.
use super::{Monitor, MonitorTable};
use crate::threading::jvm_thread::ThreadId;
use std::sync::Arc;
use std::time::Duration;

fn table() -> MonitorTable {
    let mut t = MonitorTable::new();
    t.set_vthread_monitor_unmount(true);
    t
}

fn ptr(m: &Arc<Monitor>) -> usize {
    Arc::as_ptr(m) as usize
}






