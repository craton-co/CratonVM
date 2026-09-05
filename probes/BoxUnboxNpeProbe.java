import java.util.function.Function;

/**
 * The reduced shape of `RJitLambdaNpeSupersede`, with the two things that
 * vector cannot give: a printed iteration index, so the failure names WHEN, and
 * knobs for N and for each half of the expression.
 *
 * `-Dprobe.direct=false` drops `lengthOf.apply(s)` and keeps only the
 * `stepFn` hop; `-Dprobe.step=false` does the reverse. The bug needs a
 * particular pairing, and the vector tests only both-at-once.
 */
public class BoxUnboxNpeProbe {
    private static int stepFn(Function<String, Integer> op, String v) {
        return op.apply(v);
    }

    public static void main(String[] args) {
        int n = Integer.getInteger("probe.n", 200_000);
        boolean direct = !"false".equals(System.getProperty("probe.direct"));
        boolean step = !"false".equals(System.getProperty("probe.step"));
        Function<String, Integer> lengthOf = s -> s.length();
        int sum = 0, caught = 0, spurious = 0, lastCaught = -1;
        for (int i = 0; i < n; i++) {
            String s = (i % 500 == 499) ? null : "abc";
            try {
                int v = 0;
                if (direct) {
                    v += lengthOf.apply(s);
                }
                if (step) {
                    v += stepFn(lengthOf, s);
                }
                sum += v;
            } catch (NullPointerException e) {
                caught++;
                lastCaught = i;
                if (s != null) {
                    spurious++;
                    System.out.println("SPURIOUS at i=" + i);
                }
            }
        }
        System.out.println("CK sum=" + sum + " caught=" + caught
                + " spurious=" + spurious + " lastCaught=" + lastCaught);
    }
}
