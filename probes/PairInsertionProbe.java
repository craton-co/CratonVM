/*
 * PairInsertionProbe — isolate WHICH loop of `mixedInsertionSort` has the bad
 * OSR body.
 *
 * `MixedInsertionSortProbe` reproduces the miscompile with the JDK method
 * copied verbatim. The OSR trace names `osr_bci=282`, which is the header of
 * the pair-insertion loop (the method's third region), so this probe carries
 * that loop ALONE — same source, same local layout, nothing else in the method.
 *
 * Both halves are here as separate static methods so each can be denied
 * individually (`CRATONVM_JIT_DENY=PairInsertionProbe.pairPart`), which is what
 * turns "it is somewhere in this method" into a single loop.
 *
 * Each part is a correct insertion sort of [low, high) on its own, given a
 * sentinel minimum at a[low-1] — the same precondition the JDK method relies on
 * — so each is checked against `Arrays.sort` independently.
 *
 *   <vm> -cp <out> PairInsertionProbe [n] [reps] [which]
 *     which = both (default) | pair | pin
 */

import java.util.Arrays;
import java.util.Locale;
import java.util.Random;

public class PairInsertionProbe {

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 4096;
        int reps = args.length > 1 ? Integer.parseInt(args[1]) : 400;
        String which = args.length > 2 ? args[2] : "both";
        Random r = new Random(20260804L);

        for (int rep = 0; rep < reps; rep++) {
            long[] a = new long[n];
            for (int i = 0; i < n; i++) {
                a[i] = r.nextInt(1_000_000);
            }
            a[0] = Long.MIN_VALUE; // sentinel, as the JDK method's caller guarantees
            // The pair loop consumes TWO elements per iteration (`a[++low]`),
            // so its range length must be even — the JDK's earlier regions
            // guarantee that. An odd range walks off the end on HotSpot too,
            // which is a bug in the extraction, not in the VM.
            int hi = n - ((n - 1) & 1);
            long[] expect = a.clone();
            Arrays.sort(expect, 1, hi);

            try {
                if (which.equals("pair") || which.equals("both")) {
                    pairPart(a, 1, hi);
                }
                if (which.equals("pin")) {
                    pinPart(a, 1, hi, hi);
                }
            } catch (Throwable t) {
                System.out.println(String.format(Locale.US, "PROBE-FAIL rep=%d %s", rep, t));
                return;
            }
            for (int i = 1; i < hi; i++) {
                if (a[i] != expect[i]) {
                    System.out.println(String.format(Locale.US,
                            "PROBE-FAIL rep=%d wrong at %d: got %d want %d", rep, i, a[i], expect[i]));
                    return;
                }
            }
        }
        System.out.println("PROBE-OK " + which + " " + reps + " sorts of " + n);
    }

    /** `mixedInsertionSort`'s third region, verbatim. */
    private static void pairPart(long[] a, int low, int high) {
        for (int i; low < high; ++low) {
            long a1 = a[i = low], a2 = a[++low];

            if (a1 > a2) {

                while (a1 < a[--i]) {
                    a[i + 2] = a[i];
                }
                a[++i + 1] = a1;

                while (a2 < a[--i]) {
                    a[i + 1] = a[i];
                }
                a[i + 1] = a2;

            } else if (a1 < a[i - 1]) {

                while (a2 < a[--i]) {
                    a[i + 2] = a[i];
                }
                a[++i + 1] = a2;

                while (a1 < a[--i]) {
                    a[i + 1] = a[i];
                }
                a[i + 1] = a1;
            }
        }
    }

    /** `mixedInsertionSort`'s second region, verbatim (`end` passed in). */
    private static void pinPart(long[] a, int low, int end, int high) {
        long pin = a[end - 1];

        for (int i, p = high; ++low < end; ) {
            long ai = a[i = low];

            if (ai < a[i - 1]) {

                a[i] = a[--i];

                while (ai < a[--i]) {
                    a[i + 1] = a[i];
                }
                a[i + 1] = ai;

            } else if (p > i && ai > pin) {

                while (a[--p] > pin) {
                    // find element smaller than pin
                }

                if (p > i) {
                    ai = a[p];
                    a[p] = a[i];
                }

                while (ai < a[--i]) {
                    a[i + 1] = a[i];
                }
                a[i + 1] = ai;
            }
        }
    }
}
