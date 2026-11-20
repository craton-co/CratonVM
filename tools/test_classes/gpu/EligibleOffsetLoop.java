public class EligibleOffsetLoop {
    // Non-zero (but non-negative) compile-time-constant loop start:
    // `for (i = 4; i < n; i++)`. `4` is within the `iconst_*` range
    // (`iconst_m1..iconst_5` cover -1..5), so javac emits `iconst_4`
    // here, not `bipush`/`sipush` — either way `loop_recog`'s
    // `resolve_start_value` resolves it the same way.
    //
    // Elements 0..3 of `out` are never written by the kernel — same as
    // Java, where the loop body never runs for `i < 4` — see
    // `jit-cuda/src/lowering/emit.rs`'s `apply_loop_start_offset` doc
    // comment for why that is still safe on the device side (the whole
    // array, including the untouched prefix, round-trips through an
    // unconditional full H2D upload + D2H writeback).
    //
    // `int n = a.length;` (rather than `i < a.length` inline) matches
    // every other fixture in this directory: the canonical-loop
    // recognizer requires the loop bound to be a plain `iload` in the
    // header (`iload iv; iload bound; if_icmp*`), which only happens
    // when the length is hoisted into a local first.
    public static void offsetLoop(int[] a, int[] out) {
        int n = a.length;
        for (int i = 4; i < n; i++) {
            out[i] = a[i] * 3 + 7;
        }
    }

    // Same shape, but the start value (40000) is outside `sipush`'s
    // +/-32767 range, so javac has no `iconst`/`bipush`/`sipush` short
    // form and must spill it to the constant pool, emitting `ldc`
    // instead. Exercises the AUDIT-C31-follow-up ldc-based
    // start-value resolution in `resolve_start_value`. Only the
    // *shape* is exercised here (this fixture is never actually
    // executed against a real 40000+-element array in these unit
    // tests, same as every other lowering-only fixture in this
    // directory).
    public static void offsetLoopLargeStart(int[] a, int[] out) {
        int n = a.length;
        for (int i = 40000; i < n; i++) {
            out[i] = a[i];
        }
    }
}
