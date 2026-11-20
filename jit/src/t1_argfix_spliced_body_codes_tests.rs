use super::*;

#[test]
fn t1_argfix_descriptor_param_slots_counts_wide_and_reference_slots() {
    assert_eq!(descriptor_param_slots("()V"), Some(0));
    assert_eq!(descriptor_param_slots("(Ljava/lang/Object;I)I"), Some(2));
    assert_eq!(descriptor_param_slots("(JD[I[[Ljava/lang/String;Z)V"), Some(7));
    assert_eq!(descriptor_param_slots("([J)V"), Some(1), "an array is one slot");
    assert_eq!(descriptor_param_slots("(Q)V"), None);
    assert_eq!(descriptor_param_slots("(Ljava/lang/Object"), None);
    assert_eq!(descriptor_param_slots("([)V"), None);
}

fn site(base: usize, code_len: usize, key: &str, receiver: bool) -> ir::IrInlineSite {
    ir::IrInlineSite {
        base,
        code_len,
        num_args: 1,
        max_locals: 2,
        arg_local_slots: vec![0],
        returns_value: true,
        receiver_is_arg0: receiver,
        method_key: key.to_string(),
        class_id: 7,
    }
}

/// One row per key, from the relocated bytes; the descriptor's slots plus
/// the receiver; a body whose handlers were masked uses its unmasked copy
/// with the rows rebased, and is left out when that copy is missing; a key
/// whose sites disagree is dropped.
#[test]
fn t1_argfix_spliced_body_codes_takes_the_interpreter_body_or_nothing() {
    let mut combined = vec![0u8; 40];
    combined[10..13].copy_from_slice(&[0x2b, 0xb0, 0x00]);
    combined[20..23].copy_from_slice(&[0x2b, 0xb0, 0x00]);
    combined[30..33].copy_from_slice(&[0x00, 0x00, 0xac]);
    let mut tables = ir::IrInlineTables::default();
    tables.sites.insert(1, site(10, 3, "A.g:(I)Ljava/lang/Object;", true));
    tables.sites.insert(2, site(20, 3, "A.g:(I)Ljava/lang/Object;", true));
    let rows = super::spliced_body_codes(&tables, &combined);
    assert_eq!(rows.len(), 1, "two sites of one method: one row");
    assert_eq!(rows[0].code, vec![0x2b, 0xb0, 0x00]);
    assert_eq!(rows[0].num_params, 2, "receiver + int");
    assert_eq!(rows[0].num_locals, 2);
    assert!(rows[0].handlers.is_empty());

    // Masked handlers without the unmasked copy: left out.
    let handler = ir::IrSpliceHandlerRow {
        start_pc: 30,
        end_pc: 32,
        handler_pc: 32,
        catch_name: "java/lang/Exception",
        declaring_class_id: 0,
        bare_athrow: false,
    };
    let mut masked = ir::IrInlineTables::default();
    masked.sites.insert(3, site(30, 3, "B.h:()I", false));
    masked.splice_handler_rows.insert(30, vec![handler.clone()]);
    assert!(super::spliced_body_codes(&masked, &combined).is_empty());
    // With the unmasked copy: that copy, the rows body-relative.
    masked.original_body_codes.insert(30, vec![0x04, 0x3c, 0xac]);
    let rows = super::spliced_body_codes(&masked, &combined);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].code, vec![0x04, 0x3c, 0xac]);
    assert_eq!(rows[0].handlers, vec![(0, 2, 2)]);
    assert_eq!(rows[0].num_params, 0);

    // Two sites of one key whose bytes differ: neither is trusted.
    let mut split = ir::IrInlineTables::default();
    split.sites.insert(1, site(10, 3, "C.k:()I", false));
    split.sites.insert(2, site(30, 3, "C.k:()I", false));
    assert!(super::spliced_body_codes(&split, &combined).is_empty());
}

/// Round rx27 (lane matcher): a fence-stripped body. `static void f(Object
/// x, int c) { try { if (c == 0) throw E; } finally {
/// Reference.reachabilityFence(x); } }`, the tail handler cut from the
/// spliced copy (13 of 20 bytes). The liveness row is the method's whole
/// code with its own table, so `x` (slot 0) is live at the `athrow` (pc 5),
/// whose only way on is the handler that fences it.
#[test]
fn rx27_a_fence_stripped_body_keeps_its_fenced_local_live_on_the_throw_path() {
    let original: Vec<u8> = vec![
        0x1b, // 0: iload_1
        0x9a, 0x00, 0x07, // 1: ifne -> 8
        0x01, // 4: aconst_null (the thrown value)
        0xbf, // 5: athrow
        0x00, 0x00, // 6, 7: nop
        0x2a, // 8: aload_0
        0xb8, 0x00, 0x02, // 9: invokestatic reachabilityFence
        0xb1, // 12: return
        0x4d, // 13: astore_2
        0x2a, // 14: aload_0
        0xb8, 0x00, 0x02, // 15: invokestatic reachabilityFence
        0x2c, // 18: aload_2
        0xbf, // 19: athrow
    ];
    let mut combined = vec![0u8; 10];
    combined.extend_from_slice(&original[..13]); // the stripped copy at 10..23
    let mut tables = ir::IrInlineTables::default();
    let mut s = site(10, 13, "F.f:(Ljava/lang/Object;I)V", false);
    s.max_locals = 3;
    tables.sites.insert(1, s);
    tables.original_body_codes.insert(10, original.clone());
    tables.original_body_handler_rows.insert(10, vec![(0, 8, 13, 0)]);
    let rows = super::spliced_body_codes(&tables, &combined);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].code, original, "the whole method, handler included");
    assert_eq!(rows[0].handlers, vec![(0, 8, 13)]);
    let (live, covered, words) = crate::regalloc::live_locals_per_pc_all(
        &rows[0].code,
        rows[0].code.len(),
        rows[0].num_params,
        &[],
        &rows[0].handlers,
        rows[0].num_locals,
    );
    assert!(covered[5]);
    assert_ne!(live[5 * words] & 1, 0, "x is live at the throw: the handler fences it");
}
