use super::osr_optimizing_jit_compiled_line;

#[test]
fn nio21w18_osr_optimizing_jit_compiled_line_names_the_loop() {
    assert_eq!(
        osr_optimizing_jit_compiled_line(
            "ir-osr",
            "Nio20JitRows",
            "loop",
            "(LNio20JitRows$Op;I)I",
            4,
            "cached"
        ),
        "CRATONVM_DBG_JIT_COMPILED: ir-osr Nio20JitRows.loop(LNio20JitRows$Op;I)I pc=4 (cached)"
    );
    // Never the tags the published bodies use.
    let refused = osr_optimizing_jit_compiled_line("ir-osr-refused", "A", "m", "()V", 0, "x");
    assert!(!refused.starts_with("CRATONVM_DBG_JIT_COMPILED: osr "));
    assert!(!refused.starts_with("CRATONVM_DBG_JIT_COMPILED: put "));
}
