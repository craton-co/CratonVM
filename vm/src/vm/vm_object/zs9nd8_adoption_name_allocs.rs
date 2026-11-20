use std::cell::Cell;
thread_local! {
    static N: Cell<u32> = const { Cell::new(0) };
}
pub(crate) fn note() {
    N.with(|n| n.set(n.get() + 1));
}
/// The count since the last call (and reset).
pub(crate) fn take() -> u32 {
    N.with(|n| n.replace(0))
}
