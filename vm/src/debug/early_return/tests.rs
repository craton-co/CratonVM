use super::*;

/// The return kinds JVMTI's `ForceEarlyReturn*` families accept.
#[test]
fn a_value_fits_the_return_types_of_its_kind() {
    for ret in *b"ZBCSI" {
        for tag in *b"ZBCSI" {
            assert!(kind_fits(ret, tag), "{} {}", ret as char, tag as char);
        }
        assert!(!kind_fits(ret, b'J'));
        assert!(!kind_fits(ret, b'L'));
        assert!(!kind_fits(ret, b'V'));
    }
    assert!(kind_fits(b'J', b'J') && !kind_fits(b'J', b'I') && !kind_fits(b'J', b'D'));
    assert!(kind_fits(b'F', b'F') && !kind_fits(b'F', b'D'));
    assert!(kind_fits(b'D', b'D') && !kind_fits(b'D', b'F'));
    assert!(kind_fits(b'V', b'V') && !kind_fits(b'V', b'I') && !kind_fits(b'V', b'L'));
    for tag in *b"L[stglc" {
        assert!(kind_fits(b'L', tag) && kind_fits(b'[', tag));
    }
    assert!(!kind_fits(b'L', b'I') && !kind_fits(b'[', b'V'));
}

/// A value is recorded for one park level and taken only there; the
/// points nest.
#[test]
fn a_forced_return_is_taken_at_its_own_park() {
    let shared =
        std::sync::Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));
    enter_point(&shared, 7, 3, 0);
    enter_point(&shared, 7, 5, 3);
    {
        let ds = shared.debug.debug_state.lock();
        let point = |depth, loop_base| Point {
            depth,
            loop_base,
            popped: false,
            reinvokable_entry: false,
        };
        assert_eq!(
            ds.early_returns.points.get(&7),
            Some(&vec![point(3, 0), point(5, 3)])
        );
        assert_eq!(innermost_point(&ds, 7), Some(point(5, 3)));
    }
    shared
        .debug
        .debug_state
        .lock()
        .early_returns
        .pending
        .insert(7, (3, DebuggerValue::Int(99)));
    assert_eq!(take(&shared, 7, 5), None, "recorded at the outer park");
    leave_point(&shared, 7);
    assert_eq!(take(&shared, 7, 3), Some(Some(Value::Int(99))));
    assert_eq!(take(&shared, 7, 3), None, "taken once");
    leave_point(&shared, 7);
    assert!(shared
        .debug
        .debug_state
        .lock()
        .early_returns
        .points
        .is_empty());
    shared
        .debug
        .debug_state
        .lock()
        .early_returns
        .pending
        .insert(8, (1, DebuggerValue::Void));
    assert_eq!(take(&shared, 8, 1), Some(None), "void");
}
