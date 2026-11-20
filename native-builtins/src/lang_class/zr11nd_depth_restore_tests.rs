use super::*;

/// The defect: the `getDeclaredMethods` depth was restored only by the
/// statement at the native's end, which an allocation refusal's unwind
/// skips; 51 refusals on a thread made every later listing empty.
#[test]
fn an_unwind_restores_the_reflection_depth() {
    GET_DECLARED_METHODS_DEPTH.with(|d| d.set(3));
    for _ in 0..60 {
        let unwound = std::panic::catch_unwind(|| {
            let prev = GET_DECLARED_METHODS_DEPTH.with(|d| {
                let v = d.get();
                d.set(v + 1);
                v
            });
            let _restore = DepthRestore(&GET_DECLARED_METHODS_DEPTH, prev);
            std::panic::panic_any(7u8);
        });
        assert!(unwound.is_err());
    }
    assert_eq!(GET_DECLARED_METHODS_DEPTH.with(|d| d.get()), 3);
    GET_DECLARED_METHODS_DEPTH.with(|d| d.set(0));
}
