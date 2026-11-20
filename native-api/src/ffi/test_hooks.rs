use std::sync::atomic::AtomicUsize;
/// Incremented by `NativeMemoryTable::allocate` every time the
/// ID-exhaustion path deallocates the just-allocated block before
/// returning `None`. Reset by tests that want to observe a single
/// failure in isolation.
pub(super) static EXHAUSTION_DEALLOCS: AtomicUsize = AtomicUsize::new(0);
