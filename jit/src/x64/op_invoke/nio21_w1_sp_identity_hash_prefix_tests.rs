//! nio21 wave 1, lane L5: the dispatch-arm identity-hash prefix.

#[test]
fn sp_thin_prefix_matches_identity_hash_code_helper() {
    let mut dh = crate::DirectHelperTable::default();
    dh.identity_hash_code = 0x1234_5670;
    assert_eq!(
        sp_thin_prefix_row(&dh, 0x1234_5670, false, 1, b'I'),
        Some(SpThinPrefix::IdentityHashCode)
    );
    assert_eq!(sp_thin_prefix_row(&dh, 0x1234_5670, true, 1, b'I'), None);
    assert_eq!(sp_thin_prefix_row(&dh, 0x1234_5670, false, 0, b'I'), None);
    assert_eq!(sp_thin_prefix_row(&dh, 0x1234_5670, false, 1, b'L'), None);
}

use super::*;

fn info(
    class: &'static str,
    name: &'static str,
    desc: &'static str,
    kind: u8,
) -> crate::JitInvokeInfo {
    crate::JitInvokeInfo {
        class_name: class,
        method_name: name,
        descriptor: desc,
        num_jit_args: 1,
        return_type: b'I',
        invoke_kind: kind,
        declaring_class_id: 0,
        owner_class_id: 0,
    }
}

/// `System.identityHashCode` (static) emits the optimizing tier's header
/// read byte for byte; a `Class.hashCode()` site with no `Class` id, a
/// non-static `identityHashCode` row, another method and an XMM operand
/// emit nothing.
#[test]
fn only_the_identity_hash_sites_take_the_header_read() {
    let on = sp_thin_prefix_enabled()
        && sp_identity_hash_inline_enabled()
        && crate::ir_lower::ir_identity_hash_inline_enabled()
        && !crate::x64::zgc_read_barrier_blocks_inline_fields();
    let ihc = info(
        "java/lang/System",
        "identityHashCode",
        "(Ljava/lang/Object;)I",
        3,
    );
    let mut c = super::super::emit::tests::test_compiler();
    c.stack.clear();
    c.stack.push(StackSlot::Frame(8));
    let start = c.buf.pos();
    // No `Class` id in the test table: the Class arm emits nothing.
    let class_hash = info("java/lang/Class", "hashCode", "()I", 0);
    assert!(c
        .try_emit_sp_identity_hash_dispatch_prefix(4, &class_hash)
        .is_none());
    let virtual_ihc = info(
        "java/lang/System",
        "identityHashCode",
        "(Ljava/lang/Object;)I",
        0,
    );
    assert!(c
        .try_emit_sp_identity_hash_dispatch_prefix(4, &virtual_ihc)
        .is_none());
    let other = info("java/lang/Object", "hashCode", "()I", 0);
    assert!(c
        .try_emit_sp_identity_hash_dispatch_prefix(4, &other)
        .is_none());
    c.stack[0] = StackSlot::Xmm(1);
    assert!(c
        .try_emit_sp_identity_hash_dispatch_prefix(4, &ihc)
        .is_none());
    assert_eq!(c.buf.pos(), start);
    c.stack[0] = StackSlot::Frame(8);
    let hit = c.try_emit_sp_identity_hash_dispatch_prefix(4, &ihc);
    assert_eq!(hit.is_some(), on);
    if on {
        let (read, _) = crate::ir_lower::identity_hash_read_bytes();
        let emitted = c.buf.as_slice()[start..].to_vec();
        // The read's first instructions (MOV EDX,[RAX+MARK]; MOV ECX,EDX;
        // AND ECX,STATE) appear verbatim.
        assert!(emitted.windows(8).any(|w| w == &read[..8]));
    }
}
