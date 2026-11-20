//! j27 wave 5 (lane REFSP): page
//! `j27-referent-legacy-arm-skips-the-zgc-resurrection-gate-FIXED-20261029.md`.
//! A spliced `Reference.referent` read (`InlineSite::referent_load_pcs`)
//! emits no inline arm under any barrier state: only the checked
//! `jit_getfield` carries the SATB keep-alive and the ZGC resurrection
//! gate.

use super::*;
use crate::x64::licm::{zgc_read_gate_flip_test_lock, ZgcColoredFieldLoadTestOverride};

const BASE: u64 = 0x0000_7F00_0000_0000;
const C_OFF: u32 = 40;

fn compiler() -> Compiler {
    let mut c = crate::x64::emit::tests::test_compiler();
    c.helpers.getfield = 0x7FFF_0000_2000;
    c.helpers.read_bounds_addr = 0x1000;
    c.heap_local_offset = 8;
    c.method_key = "Refsp.caller:(Ljava/lang/ref/Reference;)Ljava/lang/Object;".to_string();
    c
}

/// `(admitted, bytes emitted)` for a compact reference row of a trusted
/// receiver -- the shape every arm admits -- read as a referent or not.
fn emit(is_referent: bool) -> (bool, usize) {
    let mut c = compiler();
    let start = c.buf.pos();
    let admitted = c.emit_spliced_inline_getfield(
        StackSlot::Frame(16),
        0,
        b'L',
        Some((C_OFF, true)),
        0,
        true,
        is_referent,
        is_referent,
    );
    (admitted, c.buf.pos() - start)
}

#[test]
fn j27_refsp_a_spliced_referent_read_emits_no_inline_arm() {
    let _flip = zgc_read_gate_flip_test_lock();
    assert_eq!(emit(true), (false, 0), "unarmed: the helper, nothing inline");
    if !zgc_inline_field_loads_gated() && guarded_inline_getfield_enabled() {
        let (ok, n) = emit(false);
        assert!(ok && n > 0, "control: the same row, not a referent, reads inline");
    }
    let _open = ZgcColoredFieldLoadTestOverride::open(BASE, None);
    assert_eq!(emit(true), (false, 0), "colored: the helper, nothing inline");
}
