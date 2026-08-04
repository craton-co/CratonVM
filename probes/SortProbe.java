import java.util.Arrays;
import java.util.Random;

/** Minimal repro attempt for AIOOBE inside DualPivotQuicksort on a long[]. */
public class SortProbe {
    public static void main(String[] a) {
        int n = a.length > 0 ? Integer.parseInt(a[0]) : 20000;
        int reps = a.length > 1 ? Integer.parseInt(a[1]) : 200;
        Random r = new Random(42);
        for (int rep = 0; rep < reps; rep++) {
            long[] v = new long[n];
            for (int i = 0; i < n; i++) v[i] = r.nextInt(1_000_000);
            try {
                Arrays.sort(v);
            } catch (Throwable t) {
                System.out.println("PROBE-FAIL rep=" + rep + " " + t);
                t.printStackTrace(System.out);
                return;
            }
            for (int i = 1; i < n; i++) {
                if (v[i - 1] > v[i]) { System.out.println("PROBE-FAIL unsorted rep=" + rep + " at " + i); return; }
            }
        }
        System.out.println("PROBE-OK " + reps + " sorts of " + n + " longs");
    }
}
