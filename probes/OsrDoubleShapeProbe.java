/**
 * Minimal isolation of the `deadDoubleLocals` mismatch found by
 * OsrDeadLocalProbe: an XMM-resident local that is dead at the loop head and
 * coalesced onto a live local's XMM register.
 *
 * Prints every double the method touches, so the corrupted one names itself
 * rather than hiding inside an xor-folded checksum.
 */
public final class OsrDoubleShapeProbe {

    private static int N = 400_000;

    private static void shape(int round, double seed) {
        double pivotA = seed * 1.5;
        double pivotB = pivotA + 0.25;
        double acc0 = pivotA + pivotB;
        double sum = 0.0;
        for (int i = 0; i < N; i++) {
            sum += (i % 17) * 0.5 + acc0;
        }
        double tailA = sum / 3.0;
        double tailB = tailA - 1.0;
        System.out.println("r" + round
                + " seed=" + seed
                + " pivotA=" + pivotA
                + " pivotB=" + pivotB
                + " acc0=" + acc0
                + " sum=" + sum
                + " tailA=" + tailA
                + " tailB=" + tailB);
    }

    /** Same shape with NO dead double before the loop -- the control. */
    private static void control(int round, double seed) {
        double acc0 = seed * 1.5 + (seed * 1.5 + 0.25);
        double sum = 0.0;
        for (int i = 0; i < N; i++) {
            sum += (i % 17) * 0.5 + acc0;
        }
        System.out.println("c" + round + " acc0=" + acc0 + " sum=" + sum);
    }

    public static void main(String[] args) {
        if (args.length > 0) {
            N = Integer.parseInt(args[0]);
        }
        for (int r = 0; r < 3; r++) {
            shape(r, 3.25);
        }
        for (int r = 0; r < 3; r++) {
            control(r, 3.25);
        }
    }
}
