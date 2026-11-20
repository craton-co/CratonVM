//! Interpreter round i4 wave 89, lane L6: the unmounted `Object.wait` of
//! `CRATONVM_VTHREAD_MONITOR_UNMOUNT` (release, notify, hand-off, resume)
//! and its notify / timeout / interrupt races, at the table.
use super::{MonitorTable, VthreadWaitResume};
use crate::classloading::ClassId;
use crate::memory::heap::Heap;
use crate::threading::jvm_thread::ThreadId;
use crate::types::ObjectRef;
use std::sync::Arc;
use std::time::Duration;

fn table() -> MonitorTable {
    let mut t = MonitorTable::new();
    t.set_vthread_monitor_unmount(true);
    t
}

fn object() -> ObjectRef {
    let heap: &'static Heap = Box::leak(Box::new(Heap::with_capacity(4096)));
    heap.alloc_object(ClassId::new(0), 0)
}








