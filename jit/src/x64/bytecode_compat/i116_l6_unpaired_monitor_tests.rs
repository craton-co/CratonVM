use super::monitor_bytecode_unpaired;

#[test]
fn only_an_enter_or_an_exit_alone_is_unpaired() {
    // aload_0; monitorexit; return -- the probe's `Exit.exit`.
    assert!(monitor_bytecode_unpaired(&[0x2a, 0xc3, 0xb1], 3));
    // aload_0; monitorenter; return.
    assert!(monitor_bytecode_unpaired(&[0x2a, 0xc2, 0xb1], 3));
    // aload_0; monitorenter; aload_0; monitorexit; return.
    assert!(!monitor_bytecode_unpaired(&[0x2a, 0xc2, 0x2a, 0xc3, 0xb1], 5));
    // No monitor bytecode at all.
    assert!(!monitor_bytecode_unpaired(&[0x1a, 0xac], 2));
    // javac's `synchronized (o) { o.hashCode(); }` shape: one enter, two exits.
    let javac = [
        0x2a, 0x59, 0x4c, 0xc2, 0x2a, 0xb6, 0x00, 0x07, 0x57, 0x2b, 0xc3, 0xa7, 0x00, 0x08,
        0x4d, 0x2b, 0xc3, 0x2c, 0xbf, 0xb1,
    ];
    assert!(!monitor_bytecode_unpaired(&javac, javac.len()));
    // A `0xc3` operand byte (sipush 0x00c3) is not an instruction.
    assert!(!monitor_bytecode_unpaired(&[0x11, 0x00, 0xc3, 0xac], 4));
}
