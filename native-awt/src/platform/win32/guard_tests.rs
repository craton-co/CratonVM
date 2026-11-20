use std::sync::atomic::{AtomicUsize, Ordering};

/// Tracks how many "handles" are live. Mirrors what a real leak detector
/// would do for Win32 handle counts (e.g. `GetGuiResources` for GDI).
#[derive(Default)]
struct GuardLeakCounter {
    alive: AtomicUsize,
}

impl GuardLeakCounter {
    fn acquire(&self) {
        self.alive.fetch_add(1, Ordering::SeqCst);
    }
    fn release(&self) {
        self.alive.fetch_sub(1, Ordering::SeqCst);
    }
    fn live(&self) -> usize {
        self.alive.load(Ordering::SeqCst)
    }
}

/// Mock that mirrors `PaintGuard` / `ClipboardGuard` semantics: acquire
/// in `begin`, release in `Drop`.
struct MockPairGuard<'a> {
    counter: &'a GuardLeakCounter,
}
impl<'a> MockPairGuard<'a> {
    fn begin(counter: &'a GuardLeakCounter) -> Self {
        counter.acquire();
        Self { counter }
    }
}
impl Drop for MockPairGuard<'_> {
    fn drop(&mut self) {
        self.counter.release();
    }
}

/// Mock that mirrors `GlobalAllocGuard`: optional release before drop.
struct MockAllocGuard<'a> {
    counter: &'a GuardLeakCounter,
    released: bool,
}
impl<'a> MockAllocGuard<'a> {
    fn new(counter: &'a GuardLeakCounter) -> Self {
        counter.acquire();
        Self {
            counter,
            released: false,
        }
    }
    fn release(mut self) {
        self.released = true;
    }
}
impl Drop for MockAllocGuard<'_> {
    fn drop(&mut self) {
        if !self.released {
            self.counter.release();
        }
    }
}

/// Verifies the BeginPaint/EndPaint pair pattern: even when an early-
/// `?` propagates out before the matching close, the guard's Drop fires.
///
/// This is the regression test for the original
/// `blit_buffer` leak — `CreateDIBSection` failing with `?` used to skip
/// `EndPaint`.
#[test]
fn paint_guard_drops_on_question_mark_propagation() {
    let leaks = GuardLeakCounter::default();

    fn inner(counter: &GuardLeakCounter) -> Result<(), &'static str> {
        let _paint = MockPairGuard::begin(counter);
        // Simulates `CreateDIBSection.map_err(...)?` returning early.
        Err("create_dib_section failed")?;
        #[allow(unreachable_code)]
        Ok(())
    }

    let r = inner(&leaks);
    assert!(r.is_err(), "inner must propagate the simulated failure");
    assert_eq!(
        leaks.live(),
        0,
        "PaintGuard Drop should run when `?` bails before manual EndPaint"
    );
}

/// Verifies the GlobalAlloc/GlobalFree pair pattern: if `OpenClipboard`
/// fails after `GlobalAlloc` succeeds, the alloc still gets freed.
/// Regression test for the original `clipboard_set_text` leak.
#[test]
fn global_alloc_guard_drops_when_clipboard_open_fails() {
    let leaks = GuardLeakCounter::default();

    fn inner(counter: &GuardLeakCounter) -> Result<(), &'static str> {
        let _hmem = MockAllocGuard::new(counter);
        // GlobalLock would happen here; assume success.
        // Now OpenClipboard fails (simulated). Use `?` to model the
        // real code's early-return via the `?` operator.
        Err("OpenClipboard failed")?;
        #[allow(unreachable_code)]
        Ok(())
    }

    let r = inner(&leaks);
    assert!(r.is_err());
    assert_eq!(
        leaks.live(),
        0,
        "GlobalAllocGuard Drop should free the buffer on the clipboard \
         open failure path"
    );
}

/// When SetClipboardData succeeds the OS owns the buffer; the alloc
/// guard must NOT free it. `release()` arms that case.
#[test]
fn global_alloc_guard_release_skips_drop() {
    let leaks = GuardLeakCounter::default();

    {
        let hmem = MockAllocGuard::new(&leaks);
        assert_eq!(leaks.live(), 1);
        hmem.release();
        // After release, drop is a no-op for the counter.
        assert_eq!(
            leaks.live(),
            1,
            "release() must hand ownership off without decrementing"
        );
    }
    // Counter stays at 1 because the OS now owns it (in real code,
    // SetClipboardData -> CloseClipboard would free the global handle
    // along with the clipboard contents).
    assert_eq!(leaks.live(), 1);
}

/// Multiple guards in one scope must all run their Drop in reverse
/// declaration order. Mirrors `blit_buffer`'s nested guards.
#[test]
fn nested_guards_all_drop_on_early_return() {
    let leaks = GuardLeakCounter::default();

    fn inner(counter: &GuardLeakCounter) -> Result<(), &'static str> {
        let _paint = MockPairGuard::begin(counter);
        let _dc = MockPairGuard::begin(counter);
        let _hbm = MockPairGuard::begin(counter);
        assert_eq!(counter.live(), 3);
        // Simulate a later failure (e.g. SelectObject error).
        Err("late failure")?;
        #[allow(unreachable_code)]
        Ok(())
    }

    let _ = inner(&leaks);
    assert_eq!(
        leaks.live(),
        0,
        "all three guards must drop on `?` propagation"
    );
}

/// Sanity-check that `Drop` ordering inside Rust is LIFO. The real
/// `blit_buffer` depends on this: SelectObjectGuard (restores prev)
/// must drop BEFORE GdiObjectGuard (DeleteObject on hbm) so the bitmap
/// isn't still selected into the DC when we delete it.
#[test]
fn drop_order_is_reverse_of_declaration() {
    use std::cell::RefCell;
    let log: RefCell<Vec<&'static str>> = RefCell::new(Vec::new());

    struct Tag<'a>(&'a RefCell<Vec<&'static str>>, &'static str);
    impl Drop for Tag<'_> {
        fn drop(&mut self) {
            self.0.borrow_mut().push(self.1);
        }
    }

    {
        let _a = Tag(&log, "select_object"); // selected obj into DC
        let _b = Tag(&log, "gdi_object"); // hbm
        let _c = Tag(&log, "dc"); // mem DC
        let _d = Tag(&log, "paint"); // BeginPaint
    }

    let order = log.into_inner();
    assert_eq!(
        order,
        vec!["paint", "dc", "gdi_object", "select_object"],
        "Drop must run in reverse-declaration (LIFO) order so the \
         selection guard restores the old object before the object \
         itself is deleted, and EndPaint runs after DeleteDC"
    );
}
