/*
 * MixedInsertionSortProbe — self-contained repro for the OSR miscompile in
 * `java.util.DualPivotQuicksort.mixedInsertionSort([JII)V`.
 *
 * `probes/SortProbe.java` reproduces it through `Arrays.sort(long[])`;
 * `CRATONVM_JIT_DENY=DualPivotQuicksort.mixedInsertionSort` — and only that
 * method — makes it go away. This probe copies that method VERBATIM from the
 * JDK 25 source so the failing code can be edited and recompiled in seconds
 * instead of read out of a disassembler.
 *
 * Do not tidy the copy. The slot reuse is the point: across the method's three
 * regions the same local slots hold different widths —
 *
 *      slot 5   int i        (region 1)  /  long pin  (region 2)
 *      slot 7   long ai hi   (region 1)  /  int i     (regions 2, 3)
 *      slot 8   int j/p      (region 2)  /  long a1   (region 3)
 *
 * — which is legal JVM slot reuse across disjoint live ranges and is exactly
 * what `classify_local_kinds` calls `Ambiguous`.
 *
 *   <vm> -cp <out> MixedInsertionSortProbe [n] [reps]
 *
 * PROBE-OK / PROBE-FAIL. The sortedness check catches a wrong RESULT even when
 * no exception is thrown, which matters: a miscompile that merely misplaces an
 * element would otherwise look like a pass.
 */

import java.util.Arrays;
import java.util.Locale;
import java.util.Random;

public class MixedInsertionSortProbe {

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 4096;
        int reps = args.length > 1 ? Integer.parseInt(args[1]) : 400;
        Random r = new Random(20260804L);

        for (int rep = 0; rep < reps; rep++) {
            long[] a = new long[n];
            for (int i = 0; i < n; i++) {
                a[i] = r.nextInt(1_000_000);
            }
            // `mixedInsertionSort` is only ever called on a range whose left
            // neighbour is a proven-smaller pin, which is what stops `a[--i]`
            // from walking off the front.
            a[0] = Long.MIN_VALUE;
            long[] expect = a.clone();
            Arrays.sort(expect, 1, n);

            try {
                String v = System.getProperty("variant", "verbatim");
                switch (v) {
                    case "noreuse" -> mixedInsertionSortNoReuse(a, 1, n);
                    case "v1" -> mixedInsertionSortV1(a, 1, n);
                    case "v2" -> mixedInsertionSortV2(a, 1, n);
                    default -> mixedInsertionSort(a, 1, n);
                }
            } catch (Throwable t) {
                System.out.println(String.format(Locale.US, "PROBE-FAIL rep=%d %s", rep, t));
                return;
            }
            for (int i = 1; i < n; i++) {
                if (a[i] != expect[i]) {
                    System.out.println(String.format(Locale.US,
                            "PROBE-FAIL rep=%d wrong at %d: got %d want %d", rep, i, a[i], expect[i]));
                    return;
                }
            }
        }
        System.out.println("PROBE-OK " + System.getProperty("variant", "verbatim") + " " + reps + " sorts of " + n);
    }

    /** Verbatim from JDK 25 `java.util.DualPivotQuicksort`. */
    private static void mixedInsertionSort(long[] a, int low, int high) {
        int size = high - low;
        int end = high - 3 * ((size >> 5) << 3);
        if (end == high) {

            /*
             * Invoke simple insertion sort on tiny array.
             */
            for (int i; ++low < end; ) {
                long ai = a[i = low];

                while (ai < a[--i]) {
                    a[i + 1] = a[i];
                }
                a[i + 1] = ai;
            }
        } else {

            /*
             * Start with pin insertion sort on small part.
             */
            long pin = a[end];

            for (int i, p = high; ++low < end; ) {
                long ai = a[i = low];

                if (ai < a[i - 1]) { // Small element

                    a[i] = a[--i];

                    while (ai < a[--i]) {
                        a[i + 1] = a[i];
                    }
                    a[i + 1] = ai;

                } else if (p > i && ai > pin) { // Large element

                    while (a[--p] > pin);

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

            /*
             * Continue with pair insertion sort on remain part.
             */
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
    }

    /** Verbatim EXCEPT region 1 uses its own locals, so it stops sharing slots
     *  5/6/7 with region 2. Region 1 does not even execute at n=4096. */
    private static void mixedInsertionSortV1(long[] a, int low, int high) {
        int size = high - low;
        int end = high - 3 * ((size >> 5) << 3);
        if (end == high) {

            /*
             * Invoke simple insertion sort on tiny array.
             */
            int i1 = 0;
            long ai1 = 0;
            for (; ++low < end; ) {
                ai1 = a[i1 = low];

                while (ai1 < a[--i1]) {
                    a[i1 + 1] = a[i1];
                }
                a[i1 + 1] = ai1;
            }
        } else {

            /*
             * Start with pin insertion sort on small part.
             */
            long pin = a[end];

            for (int i, p = high; ++low < end; ) {
                long ai = a[i = low];

                if (ai < a[i - 1]) { // Small element

                    a[i] = a[--i];

                    while (ai < a[--i]) {
                        a[i + 1] = a[i];
                    }
                    a[i + 1] = ai;

                } else if (p > i && ai > pin) { // Large element

                    while (a[--p] > pin);

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

            /*
             * Continue with pair insertion sort on remain part.
             */
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
    }


    /** Verbatim EXCEPT region 3 uses its own locals, so it stops sharing
     *  slots 7/8/9 with region 2. */
    private static void mixedInsertionSortV2(long[] a, int low, int high) {
        int size = high - low;
        int end = high - 3 * ((size >> 5) << 3);
        if (end == high) {
            for (int i; ++low < end; ) {
                long ai = a[i = low];
                while (ai < a[--i]) {
                    a[i + 1] = a[i];
                }
                a[i + 1] = ai;
            }
        } else {
            long pin = a[end];

            for (int i, p = high; ++low < end; ) {
                long ai = a[i = low];

                if (ai < a[i - 1]) {
                    a[i] = a[--i];
                    while (ai < a[--i]) {
                        a[i + 1] = a[i];
                    }
                    a[i + 1] = ai;
                } else if (p > i && ai > pin) {
                    while (a[--p] > pin);
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

            // Region 3 with its OWN locals.
            int i3 = 0;
            long b1 = 0, b2 = 0;
            for (; low < high; ++low) {
                b1 = a[i3 = low];
                b2 = a[++low];

                if (b1 > b2) {
                    while (b1 < a[--i3]) {
                        a[i3 + 2] = a[i3];
                    }
                    a[++i3 + 1] = b1;

                    while (b2 < a[--i3]) {
                        a[i3 + 1] = a[i3];
                    }
                    a[i3 + 1] = b2;
                } else if (b1 < a[i3 - 1]) {
                    while (b2 < a[--i3]) {
                        a[i3 + 2] = a[i3];
                    }
                    a[++i3 + 1] = b2;

                    while (b1 < a[--i3]) {
                        a[i3 + 1] = a[i3];
                    }
                    a[i3 + 1] = b1;
                }
            }
        }
    }

    /** Same algorithm, but every local declared up front so NO slot is reused. */
    private static void mixedInsertionSortNoReuse(long[] a, int low, int high) {
        // Hoisted: distinct slots for every value, so no slot carries two
        // widths across the method's three regions.
        int i1 = 0; long ai1 = 0;
        long pin = 0; int i2 = 0, p = 0; long ai2 = 0;
        int i3 = 0; long a1 = 0, a2 = 0;
        int size = high - low;
        int end = high - 3 * ((size >> 5) << 3);
        if (end == high) {

            /*
             * Invoke simple insertion sort on tiny array.
             */
            for (; ++low < end; ) {
                ai1 = a[i1 = low];

                while (ai1 < a[--i1]) {
                    a[i1 + 1] = a[i1];
                }
                a[i1 + 1] = ai1;
            }
        } else {

            /*
             * Start with pin insertion sort on small part.
             */
            pin = a[end];

            for (p = high; ++low < end; ) {
                ai2 = a[i2 = low];

                if (ai2 < a[i2 - 1]) { // Small element

                    a[i2] = a[--i2];

                    while (ai2 < a[--i2]) {
                        a[i2 + 1] = a[i2];
                    }
                    a[i2 + 1] = ai2;

                } else if (p > i2 && ai2 > pin) { // Large element

                    while (a[--p] > pin);

                    if (p > i2) {
                        ai2 = a[p];
                        a[p] = a[i2];
                    }

                    while (ai2 < a[--i2]) {
                        a[i2 + 1] = a[i2];
                    }
                    a[i2 + 1] = ai2;
                }
            }

            /*
             * Continue with pair insertion sort on remain part.
             */
            for (; low < high; ++low) {
                a1 = a[i3 = low]; a2 = a[++low];

                if (a1 > a2) {

                    while (a1 < a[--i3]) {
                        a[i3 + 2] = a[i3];
                    }
                    a[++i3 + 1] = a1;

                    while (a2 < a[--i3]) {
                        a[i3 + 1] = a[i3];
                    }
                    a[i3 + 1] = a2;

                } else if (a1 < a[i3 - 1]) {

                    while (a2 < a[--i3]) {
                        a[i3 + 2] = a[i3];
                    }
                    a[++i3 + 1] = a2;

                    while (a1 < a[--i3]) {
                        a[i3 + 1] = a[i3];
                    }
                    a[i3 + 1] = a1;
                }
            }
        }
    }
}
