use super::SharedVm;
use std::cell::RefCell;

type Observer = Box<dyn Fn(&SharedVm, &[(usize, usize)])>;

thread_local! {
    static OBSERVER: RefCell<Option<Observer>> = const { RefCell::new(None) };
}

pub(crate) fn set(observer: Option<Observer>) {
    OBSERVER.with(|o| *o.borrow_mut() = observer);
}

pub(crate) fn observe(shared: &SharedVm, dead: &[(usize, usize)]) {
    OBSERVER.with(|o| {
        if let Some(f) = o.borrow().as_ref() {
            f(shared, dead);
        }
    });
}
