use super::{unsafe_park_absolute_until_field, unsafe_park_returns_at_once, unsafe_park_timeout};

const NOW: i64 = 1_760_000_000_000;
const SRC: &str = concat!(include_str!("../unsafe_natives_ext/mod.rs"), "\n", include_str!("../unsafe_natives_ext/unsafe_offset_is_heap_slot.rs"), "\n", include_str!("../unsafe_natives_ext/native_unsafe_get_long_volatile.rs"), "\n", include_str!("../unsafe_natives_ext/unsafe_array_read_bytes.rs"));

fn body(sig: &str) -> &'static str {
    let start = SRC.find(sig).unwrap_or_else(|| panic!("`{sig}` not found"));
    let len = SRC[start..].find("\n\u{7d}\n").expect("its end");
    &SRC[start..start + len]
}

/// Every argument pair `Parker::park` returns at once for is answered
/// "at once" here; an untimed or a pending timed park is not.
#[test]
fn gci_i22rv_the_at_once_answers_are_exactly_hotspots_immediate_returns() {
    for (abs, time) in [
        (false, -1),
        (false, i64::MIN),
        (true, 0),
        (true, -1),
        (true, i64::MIN),
        (true, NOW - 5),
        (true, NOW),
    ] {
        assert!(
            unsafe_park_returns_at_once(unsafe_park_timeout(abs, time, NOW)),
            "abs={abs} time={time}"
        );
    }
    for (abs, time) in [(false, 0), (false, 1), (false, 50_000), (true, NOW + 250)] {
        assert!(
            !unsafe_park_returns_at_once(unsafe_park_timeout(abs, time, NOW)),
            "abs={abs} time={time}"
        );
    }
}

/// The immediate return is decided with the interrupt test, before the
/// virtual-thread yield: `try_yield_virtual_park` turns `Some(ZERO)` into
/// `wake_after_nanos: 0`, the untimed yield.
#[test]
fn gci_i22rv_an_elapsed_deadline_never_reaches_the_virtual_yield() {
    let park = body("fn native_unsafe_park(");
    let gate = park
        .find("if ctx.is_interrupted(false) || unsafe_park_returns_at_once(timeout) \u{7b}")
        .expect("the at-once gate");
    let door = park[gate..]
        .find("unsafe_park_door(ctx, timeout, is_absolute, time);")
        .map(|i| gate + i)
        .expect("the gate's door");
    let yielded = park
        .find("try_yield_virtual_park(ctx, timeout)")
        .expect("the yield");
    assert!(gate < door && door < yielded, "{gate} {door} {yielded}");
    // The `LockSupport.parkUntil(Object, long)` stand-in (`--compatible`
    // with `CRATONVM_COMPAT_JDK_LOCK_SUPPORT=0`, or the synthetic library):
    // the same rule, by its remaining milliseconds.
    const LS: &str = concat!(include_str!("../util_concurrent_ext/mod.rs"), "\n", include_str!("../util_concurrent_ext/register_queue_bridge_natives.rs"), "\n", include_str!("../util_concurrent_ext/register_t31_concurrent_extras.rs"), "\n", include_str!("../util_concurrent_ext/native_stamped_unlock_by_stamp.rs"));
    let start = LS
        .find("fn native_lock_support_park_until(")
        .expect("the stand-in");
    let len = LS[start..].find("\n\u{7d}\n").expect("its end");
    let until = &LS[start..start + len];
    let guard = until.find("if remaining > 0 \u{7b}").expect("the guard");
    let yield_at = until
        .find("try_yield_virtual_park(ctx, timeout)")
        .expect("the yield");
    assert!(guard < yield_at);
}

/// `Unsafe_Park` posts `time == 0` as an untimed park (`until =
/// Long.MIN_VALUE`) in both forms; any other absolute time as `until`.
#[test]
fn gci_i22rv_an_absolute_zero_posts_an_untimed_park_event() {
    assert_eq!(unsafe_park_absolute_until_field(0), i64::MIN);
    assert_eq!(unsafe_park_absolute_until_field(-1), -1);
    assert_eq!(unsafe_park_absolute_until_field(NOW), NOW);
    assert_eq!(unsafe_park_absolute_until_field(i64::MIN), i64::MIN);
    let door = body("fn unsafe_park_door(");
    assert!(
        door.contains("ctx.park_until_reporting(timeout, unsafe_park_absolute_until_field(time));"),
        "{door}"
    );
}
