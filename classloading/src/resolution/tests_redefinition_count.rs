use super::ClassId;
use std::cell::{Cell, RefCell};
thread_local! {
    static COUNT: Cell<u64> = const { Cell::new(0) };
    /// `(count, class)` per bump; `None` for a bump that named no class.
    static RECORDS: RefCell<Vec<(u64, Option<ClassId>)>> = const { RefCell::new(Vec::new()) };
}
pub(super) fn get() -> u64 {
    COUNT.with(Cell::get)
}
/// What an unattributed bump does to the real counter, on this thread
/// only: its record retires everything.
pub(super) fn bump() {
    record(None);
}
/// What `redefine_class` of `class` does to the real counter and ring,
/// on this thread only.
pub(super) fn bump_of(class: ClassId) {
    record(Some(class));
}
fn record(class: Option<ClassId>) {
    let next = get() + 1;
    RECORDS.with(|r| r.borrow_mut().push((next, class)));
    COUNT.with(|c| c.set(next));
}
/// Forget the records older than the last `keep` counts: what the real
/// ring does once it laps a reader.
pub(super) fn lap(keep: u64) {
    let floor = get().saturating_sub(keep);
    RECORDS.with(|r| r.borrow_mut().retain(|(count, _)| *count > floor));
}
pub(super) fn for_each_between(after: u64, upto: u64, mut visit: impl FnMut(ClassId)) -> bool {
    RECORDS.with(|r| {
        let records = r.borrow();
        for count in after + 1..=upto {
            match records.iter().find(|(c, _)| *c == count) {
                Some((_, Some(class))) => visit(*class),
                _ => return false,
            }
        }
        true
    })
}
