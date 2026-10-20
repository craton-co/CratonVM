// Companion to BoundsDeopt2: calls the mismatched-length vectorAdd REPEATEDLY
// so the speculative-BCE header guard deopts past the per-bci de-spec
// threshold and the method is recompiled WITHOUT the guard. The recompile
// must restore the per-element bounds checks for the accesses that guard had
// covered (SpeculativeBCEGuard::covered_pcs) — a de-spec'd recompile that
// kept the elisions would silently store past `out`'s end on every
// subsequent call. Every call, before and after de-spec, must throw
// ArrayIndexOutOfBoundsException at i = out.length.
public class BoundsDeopt3 {
    public static void main(String[] args) {
        int n = 1 << 20;
        int[] a = new int[n];
        int[] b = new int[n];
        int[] out = new int[n / 2];
        int thrown = 0, silent = 0;
        for (int call = 0; call < 40; call++) {
            try {
                EligibleVectorAdd.vectorAdd(a, b, out);
                silent++;
            } catch (ArrayIndexOutOfBoundsException e) {
                thrown++;
            }
        }
        System.out.println((silent == 0 ? "PASS" : "FAIL")
            + " thrown=" + thrown + " silent=" + silent);
    }
}
