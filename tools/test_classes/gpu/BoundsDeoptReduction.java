/**
 * A REDUCTION that fails its bounds check.
 *
 * The reduction epilogue's block stage has a {@code bar.sync}, and a
 * thread that returns from the middle of the kernel while the rest of
 * its block waits at that barrier hangs the launch. The bounds-check
 * deopt used to do exactly that, so it now raises the failure flag and
 * branches into the reduction's zero label instead — contributing
 * nothing and arriving at the barrier with everyone else.
 *
 * {@code BoundsDeopt2} covers the same path for an element-wise kernel,
 * which has no barrier and therefore cannot show this.
 *
 * Expected: the VM notices the raised flag, throws away the GPU result
 * and re-runs on the CPU, so this prints the same answer HotSpot does —
 * and, above all, it TERMINATES.
 */
public class BoundsDeoptReduction {
    public static void main(String[] args) {
        int n = 1 << 20;
        int[] a = new int[n];
        // Shorter than `a`, so the kernel's index into `b` is out of
        // range and the precondition the emitter hoists cannot hold.
        int[] b = new int[n / 2];
        for (int i = 0; i < n; i++) {
            a[i] = i % 1024;
        }
        for (int i = 0; i < b.length; i++) {
            b[i] = 3;
        }
        long got;
        String how;
        try {
            got = EligibleDotProduct.dot(a, b);
            how = "NO-EXCEPTION";
        } catch (Throwable e) {
            got = Long.MIN_VALUE;
            how = "THROWN " + e.getClass().getName();
        }
        // What the JLS says the same loop does on the CPU.
        long want;
        String wantHow;
        try {
            long s = 0;
            for (int i = 0; i < a.length; i++) {
                s += (long) a[i] * b[i];
            }
            want = s;
            wantHow = "NO-EXCEPTION";
        } catch (Throwable e) {
            want = Long.MIN_VALUE;
            wantHow = "THROWN " + e.getClass().getName();
        }
        System.out.println("REDUCTIONDEOPT got=" + how + " want=" + wantHow
                + " value_ok=" + (got == want)
                + " ok=" + (how.equals(wantHow) && got == want));
    }
}
