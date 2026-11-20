use super::native_unsafe_park;
use crate::test_utils::MockNativeContext;
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::Value;

const SRC: &str = concat!(include_str!("../unsafe_natives_ext/mod.rs"), "\n", include_str!("../unsafe_natives_ext/unsafe_offset_is_heap_slot.rs"), "\n", include_str!("../unsafe_natives_ext/native_unsafe_get_long_volatile.rs"), "\n", include_str!("../unsafe_natives_ext/unsafe_array_read_bytes.rs"));

/// The body of the first `fn <name>(` in this file (the definition precedes
/// the tests that name it), up to its closing line.
fn body(name: &str) -> &'static str {
    let start = SRC.find(name).expect("the function is in this file");
    let len = SRC[start..].find("\n\u{7d}\n").expect("the function ends");
    &SRC[start..start + len]
}

fn at(text: &str, needle: &str) -> usize {
    text.find(needle).unwrap_or_else(|| panic!("missing: {needle}"))
}

fn no_java(
    _ctx: &mut MockNativeContext,
    class: &str,
    name: &str,
    _desc: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    panic!("a platform thread's park ran Java: {class}.{name}");
}

/// The order inside the native: the yield attempt, then the carrier test,
/// the learned and carrier gates, `Blocker.begin`, the park, and `Blocker.end` only when
/// compensation was attempted.
#[test]
fn gci_i17vt_the_live_park_compensates_around_the_park_only() {
    let park = body("fn native_unsafe_park(");
    let yielded = at(park, "try_yield_virtual_park(ctx, timeout)");
    let carrier = at(park, "ctx.blocks_jdk_carrier()");
    let learned = at(park, "!short_timed && ctx.vm_pinned_park_compensates()");
    let begin = at(park, "ctx.begin_carrier_compensation()");
    let door = at(park, "unsafe_park_door(ctx, timeout, is_absolute, time);\n    if compensated");
    let end = at(park, "ctx.end_carrier_compensation(true)");
    assert!(yielded < learned && learned < carrier, "a park that yields never compensates");
    assert!(carrier < begin && park[learned..begin].contains("||"), "either gate can request compensation");
    assert!(begin < door && door < end, "Blocker.end follows the park");
}

