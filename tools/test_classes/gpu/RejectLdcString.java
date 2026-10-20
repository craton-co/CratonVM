public class RejectLdcString {
    // AUDIT C31 follow-up (2026-07-11): a `String` constant-pool entry
    // must still be rejected even now that the analyzer resolves
    // ldc/ldc2_w targets — only Integer/Float/Long/Double are
    // GPU-representable immediates. `s` is never read after the store,
    // so javac emits exactly `ldc #<String "x">; astore_0; return` —
    // the `ldc` is the very first instruction, so whichever reject
    // reason fires is unambiguously about the ldc classification, not
    // (say) the astore of a reference-typed local.
    public static void noop() {
        String s = "x";
    }
}
