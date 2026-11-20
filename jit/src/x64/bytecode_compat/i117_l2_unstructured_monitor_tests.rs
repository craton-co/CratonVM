use super::monitor_bytecode_unstructured as unstructured;

#[test]
fn javac_synchronized_block_is_structured() {
    // synchronized (o) { o.hashCode(); }   (o in local 0, lock in local 1)
    //  0 aload_0; 1 dup; 2 astore_1; 3 monitorenter; 4 aload_0;
    //  5 invokevirtual #7; 8 pop; 9 aload_1; 10 monitorexit; 11 goto +8 (19);
    // 14 astore_2; 15 aload_1; 16 monitorexit; 17 aload_2; 18 athrow; 19 return
    let code = [
        0x2a, 0x59, 0x4c, 0xc2, 0x2a, 0xb6, 0x00, 0x07, 0x57, 0x2b, 0xc3, 0xa7, 0x00, 0x08,
        0x4d, 0x2b, 0xc3, 0x2c, 0xbf, 0xb1,
    ];
    assert!(!unstructured(&code, code.len()));
}

#[test]
fn javac_return_inside_and_nested_blocks_are_structured() {
    // synchronized (a) { synchronized (b) { return 1; } }
    //  0 aload_0; dup; astore_2; monitorenter        (lock a in 2)
    //  4 aload_1; dup; astore_3; monitorenter        (lock b in 3)
    //  8 iconst_1; aload_3; monitorexit; aload_2; monitorexit; ireturn
    // 14 handler: astore 4; aload_3; monitorexit; aload 4; athrow   (not walked)
    let code = [
        0x2a, 0x59, 0x4d, 0xc2, 0x2b, 0x59, 0x4e, 0xc2, 0x04, 0x2d, 0xc3, 0x2c, 0xc3, 0xac,
        0x3a, 0x04, 0x2d, 0xc3, 0x19, 0x04, 0xbf,
    ];
    assert!(!unstructured(&code, code.len()));
}

#[test]
fn javac_block_in_a_loop_is_structured() {
    // for (;;) { if (c) break; synchronized (o) { } }   o in 0, c in 1, lock in 2
    //  0 iload_1; ifne +12 (13); aload_0; dup; astore_2; monitorenter;
    //  8 aload_2; monitorexit; goto -10 (0); 13 return
    let code = [
        0x1b, 0x9a, 0x00, 0x0c, 0x2a, 0x59, 0x4d, 0xc2, 0x2c, 0xc3, 0xa7, 0xff, 0xf6, 0xb1,
    ];
    assert!(!unstructured(&code, code.len()));
}

#[test]
fn an_exit_of_another_object_is_unstructured() {
    // aload_0; monitorenter; aload_1; monitorexit; return
    assert!(unstructured(&[0x2a, 0xc2, 0x2b, 0xc3, 0xb1], 5));
}

#[test]
fn a_lock_held_at_return_is_unstructured() {
    // aload_0; monitorenter; aload_0; monitorexit; aload_0; monitorenter; return
    assert!(unstructured(&[0x2a, 0xc2, 0x2a, 0xc3, 0x2a, 0xc2, 0xb1], 7));
}

#[test]
fn an_exit_with_nothing_held_is_unstructured() {
    // aload_0; monitorenter; aload_0; monitorexit; aload_0; monitorexit; return
    assert!(unstructured(&[0x2a, 0xc2, 0x2a, 0xc3, 0x2a, 0xc3, 0xb1], 7));
}

#[test]
fn a_depth_that_depends_on_the_path_is_unstructured() {
    // iload_1; ifeq +6 (7); aload_0; monitorenter; nop; 7 aload_0; monitorexit; return
    let code = [0x1b, 0x99, 0x00, 0x06, 0x2a, 0xc2, 0x00, 0x2a, 0xc3, 0xb1];
    assert!(unstructured(&code, code.len()));
}

#[test]
fn unknown_operands_and_no_monitors_are_not_refused() {
    // getstatic #1; monitorenter; getstatic #1; monitorexit; return
    let code = [0xb2, 0x00, 0x01, 0xc2, 0xb2, 0x00, 0x01, 0xc3, 0xb1];
    assert!(!unstructured(&code, code.len()));
    assert!(!unstructured(&[0x1a, 0xac], 2));
    // An exit of a local stored over while held: unknown, not refused.
    // aload_0; astore_1; aload_1; monitorenter; aload_0; astore_1; aload_1; monitorexit; return
    let code = [0x2a, 0x4c, 0x2b, 0xc2, 0x2a, 0x4c, 0x2b, 0xc3, 0xb1];
    assert!(!unstructured(&code, code.len()));
}
