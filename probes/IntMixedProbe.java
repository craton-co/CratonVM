import java.util.Arrays;
import java.util.Locale;
import java.util.Random;

/**
 * The same method shape as `mixedInsertionSort`, but over `int[]` — so the
 * locals reuse slots exactly as before while NO slot is ever cat-2.
 *
 * If this passes where the `long[]` version fails, the defect needs a cat-2
 * local in the reuse set, not merely reuse.
 */
public class IntMixedProbe {
    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 4096;
        int reps = args.length > 1 ? Integer.parseInt(args[1]) : 200;
        Random r = new Random(20260804L);
        for (int rep = 0; rep < reps; rep++) {
            int[] a = new int[n];
            for (int i = 0; i < n; i++) a[i] = r.nextInt(1_000_000);
            a[0] = Integer.MIN_VALUE;
            int[] expect = a.clone();
            Arrays.sort(expect, 1, n);
            try {
                mixedInsertionSort(a, 1, n);
            } catch (Throwable t) {
                System.out.println(String.format(Locale.US, "PROBE-FAIL rep=%d %s", rep, t));
                return;
            }
            for (int i = 1; i < n; i++) {
                if (a[i] != expect[i]) {
                    System.out.println(String.format(Locale.US,
                            "PROBE-FAIL rep=%d wrong at %d", rep, i));
                    return;
                }
            }
        }
        System.out.println("PROBE-OK int " + reps + " sorts of " + n);
    }

    private static void mixedInsertionSort(int[] a, int low, int high) {
        int size = high - low;
        int end = high - 3 * ((size >> 5) << 3);
        if (end == high) {
            for (int i; ++low < end; ) {
                int ai = a[i = low];
                while (ai < a[--i]) { a[i + 1] = a[i]; }
                a[i + 1] = ai;
            }
        } else {
            int pin = a[end];
            for (int i, p = high; ++low < end; ) {
                int ai = a[i = low];
                if (ai < a[i - 1]) {
                    a[i] = a[--i];
                    while (ai < a[--i]) { a[i + 1] = a[i]; }
                    a[i + 1] = ai;
                } else if (p > i && ai > pin) {
                    while (a[--p] > pin);
                    if (p > i) { ai = a[p]; a[p] = a[i]; }
                    while (ai < a[--i]) { a[i + 1] = a[i]; }
                    a[i + 1] = ai;
                }
            }
            for (int i; low < high; ++low) {
                int a1 = a[i = low], a2 = a[++low];
                if (a1 > a2) {
                    while (a1 < a[--i]) { a[i + 2] = a[i]; }
                    a[++i + 1] = a1;
                    while (a2 < a[--i]) { a[i + 1] = a[i]; }
                    a[i + 1] = a2;
                } else if (a1 < a[i - 1]) {
                    while (a2 < a[--i]) { a[i + 2] = a[i]; }
                    a[++i + 1] = a2;
                    while (a1 < a[--i]) { a[i + 1] = a[i]; }
                    a[i + 1] = a1;
                }
            }
        }
    }
}
