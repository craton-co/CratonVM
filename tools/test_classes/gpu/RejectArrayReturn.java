public class RejectArrayReturn {
    // An array-returning kernel that allocates NOTHING: the output
    // buffer is a parameter and the method hands it straight back.
    // Compiles to `... aload_0; areturn` — no `newarray`, so the
    // allocation band never sees it.
    //
    // The analyzer used to admit this: `[I` is a legal return kind
    // (ParamKind::I32Array) and `areturn` (0xB0) sits inside the
    // permitted `0xAC..=0xB1` band. `lowering/emit.rs` has no arm for
    // 0xB0, so it was analyzed-Eligible and then lowering-rejected.
    // See `analyzer::Reason::UnsupportedReturnType`.
    public static int[] doubleInPlace(int[] a) {
        int n = a.length;
        for (int i = 0; i < n; i++) {
            a[i] = a[i] * 2;
        }
        return a;
    }
}
