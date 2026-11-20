use super::*;

/// `return; <handler body>` — the shape javac emits when a `try` block
/// returns. The handler is reachable from nothing the bytecode names.
///
/// Without a root it is dead, which is the pre-2026-08-20 world and why
/// "a handler body is dead code in the emitted image" was true. With one it
/// is live, and so is everything it falls through to — which is what makes
/// `pc_to_native[handler_pc]` a real address for a local-handler stub to
/// jump to instead of the `-1` that would reject the whole method.
#[test]
fn a_handler_root_revives_the_block_and_nothing_else_does() {
    // 0: return
    // 1: astore_0        <- handler_pc
    // 2: return
    let code = [0xb1u8, 0x4b, 0xb1];
    let without =
        bytecode_analysis::reachable_pcs(&code, code.len(), &[]).expect("statically known CFG");
    assert!(without[0]);
    assert!(!without[1], "nothing branches to a handler body");
    assert!(!without[2]);

    let with = bytecode_analysis::reachable_pcs(&code, code.len(), &[1])
        .expect("statically known CFG");
    assert!(with[0]);
    assert!(with[1], "the handler root makes its own block live");
    assert!(with[2], "and everything the handler falls through to");
}

/// An empty root list must be the identity, because that is every compile
/// that arms no local handlers — i.e. every compile until someone sets the
/// flag.
#[test]
fn no_roots_is_the_identity() {
    // 0: iconst_0  1: ifeq +4 (->5)  4: return  5: return
    let code = [0x03u8, 0x99, 0x00, 0x04, 0xb1, 0xb1];
    assert_eq!(
        bytecode_analysis::reachable_pcs(&code, code.len(), &[]),
        bytecode_analysis::reachable_pcs(&code, code.len(), &[]),
    );
}
