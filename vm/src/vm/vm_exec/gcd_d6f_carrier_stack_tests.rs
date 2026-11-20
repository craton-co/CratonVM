use super::{carrier_stack_bytes, DEFAULT_CARRIER_STACK_BYTES};

const MIB: usize = 1024 * 1024;

/// No request, a negative one, or one the default covers: the default,
/// exactly as before (the byte-for-byte default the d6/f brief requires).
#[test]
fn a_request_the_default_covers_keeps_the_default() {
    for requested in [0i64, -1, i64::MIN, 256 * 1024, 1 << 20, 8 << 20] {
        assert_eq!(
            carrier_stack_bytes(DEFAULT_CARRIER_STACK_BYTES, requested),
            DEFAULT_CARRIER_STACK_BYTES,
            "requested {requested}"
        );
    }
    // `RUST_MIN_STACK` dialled down: still the dialled value for no request.
    assert_eq!(carrier_stack_bytes(2 * MIB, 0), 2 * MIB);
}

/// A larger request gets the request on top of the default budget,
/// rounded up to 1 MiB, clamped at 1 GiB of request.
#[test]
fn a_larger_request_is_honoured_on_top_of_the_default() {
    assert_eq!(
        carrier_stack_bytes(DEFAULT_CARRIER_STACK_BYTES, 256 << 20),
        (256 + 8) * MIB
    );
    assert_eq!(
        carrier_stack_bytes(DEFAULT_CARRIER_STACK_BYTES, (16 << 20) + 1),
        (16 + 8 + 1) * MIB
    );
    assert_eq!(
        carrier_stack_bytes(DEFAULT_CARRIER_STACK_BYTES, 1 << 30),
        1024 * MIB + DEFAULT_CARRIER_STACK_BYTES
    );
    assert_eq!(
        carrier_stack_bytes(DEFAULT_CARRIER_STACK_BYTES, i64::MAX),
        1024 * MIB + DEFAULT_CARRIER_STACK_BYTES,
        "Long.MAX_VALUE is clamped, not handed to the OS"
    );
    // A small `RUST_MIN_STACK` with a large request: the request wins.
    assert_eq!(carrier_stack_bytes(MIB, 64 << 20), (64 + 8) * MIB);
}

/// JIT round 23 wave 2, lane L3: the main thread's compiled-recursion
/// growth for a `-Xss` default is the growth a platform thread started
/// without a `stackSize` gets from it; no `-Xss` is `0`, as before.
#[test]
fn the_main_thread_grows_its_budget_for_xss_like_any_thread() {
    use super::main_thread_self_call_extra_budget as budget;
    assert_eq!(budget(None), 0);
    assert_eq!(budget(Some(1 << 20)), 0);
    assert_eq!(budget(Some(8 << 20)), 0);
    assert_eq!(budget(Some(16 << 20)), 16 * MIB);
    assert_eq!(budget(Some((16 << 20) + 1)), 17 * MIB);
    assert_eq!(budget(Some(usize::MAX)), 1024 * MIB);
}
