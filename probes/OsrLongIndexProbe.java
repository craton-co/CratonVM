/*
 * OsrLongIndexProbe — minimal repro for the OSR miscompile that makes
 * `Arrays.sort(long[])` throw a bogus ArrayIndexOutOfBoundsException.
 *
 * Found 2026-08-03 while trying to run `probes/HandoffLayersProbe.java`, which
 * sorts a `long[]` of samples: it died with
 *
 *   ArrayIndexOutOfBoundsException: Index -621629441 out of bounds for length 20000
 *     at java.util.DualPivotQuicksort.mixedInsertionSort(...)
 *
 * The index is garbage; the array is the right length. `probes/SortProbe.java`
 * reduces it to `Arrays.sort(new long[n])` for n >= 1000, and narrows it:
 *
 *   --nojit                          PASS
 *   CRATONVM_JIT_OSR=0               PASS
 *   CRATONVM_JIT_DENY=java/...       still FAILS  (the deny filter does not
 *   CRATONVM_JIT_BISECT_ONLY=<none>  still FAILS   reach the OSR compile path)
 *   CRATONVM_JIT_OSR_DEAD_LOCALS=0   still FAILS
 *   CRATONVM_JIT_OSR_SINGLE_PC=1     still FAILS  (so it is not the artifact
 *                                                  being entered at a second
 *                                                  loop header)
 *   --Xmx 12g                        still FAILS  (so it is not the collector)
 *
 * i.e. the OSR-compiled BODY is wrong. This probe is the shape:
 * `DualPivotQuicksort.mixedInsertionSort`'s inner loop, which combines
 *
 *   - a `long[]` element read into a `long` local,
 *   - an `int` index assigned MID-EXPRESSION (`a[i = low]` — `dup`/`istore`),
 *   - a pre-decrement of that index inside the loop condition (`a[--i]`),
 *
 * so an OSR entry must restore an `int` operand-stack/local value in a method
 * that also holds a `long`. That is exactly the width-source distinction the
 * OSR typed-operand-stack work addresses; a wrong width there restores a
 * garbage index, which is what the exception reports.
 *
 *   <vm> -cp <out> OsrLongIndexProbe [n] [reps]
 *
 * PROBE-OK / PROBE-FAIL, and the sorted-ness check catches a wrong RESULT even
 * when no exception is thrown.
 */

import java.util.Locale;
import java.util.Random;

public class OsrLongIndexProbe {

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 20000;
        int reps = args.length > 1 ? Integer.parseInt(args[1]) : 50;
        Random r = new Random(20260803L);
        for (int rep = 0; rep < reps; rep++) {
            long[] a = new long[n];
            for (int i = 0; i < n; i++) {
                a[i] = r.nextInt(1_000_000);
            }
            // The real `mixedInsertionSort` is only ever called on a range
            // whose left neighbour is a proven-smaller "pin", which is what
            // stops `a[--i]` walking off the front. Reproduce that precondition
            // rather than adding a bound to the loop — the missing bound IS the
            // bytecode shape under test.
            a[0] = Long.MIN_VALUE;
            try {
                insertionSort(a, 1, n);
            } catch (Throwable t) {
                System.out.println(String.format(Locale.US,
                        "PROBE-FAIL rep=%d %s", rep, t));
                return;
            }
            for (int i = 1; i < n; i++) {
                if (a[i - 1] > a[i]) {
                    System.out.println(String.format(Locale.US,
                            "PROBE-FAIL rep=%d unsorted at %d (%d > %d)", rep, i, a[i - 1], a[i]));
                    return;
                }
            }
        }
        System.out.println("PROBE-OK " + reps + " sorts of " + n);
    }

    /**
     * `java.util.DualPivotQuicksort.mixedInsertionSort`'s insertion loop,
     * verbatim in shape. Do not "clean up" the mid-expression `i = low` or the
     * `--i` in the condition — those are the bytecode shapes under test.
     */
    private static void insertionSort(long[] a, int low, int high) {
        for (int i; ++low < high; ) {
            long ai = a[i = low];
            while (ai < a[--i]) {
                a[i + 1] = a[i];
            }
            a[i + 1] = ai;
        }
    }
}
