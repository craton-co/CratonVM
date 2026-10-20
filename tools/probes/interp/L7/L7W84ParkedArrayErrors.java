// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 84, lane L7: the dispatch loop's parked
// fast-path error (`PendingRuntimeError`, boxed since wave 84 to take 56
// bytes out of `execute_frame_from_index`'s frame) must raise exactly what
// it raised inline. Every array arm that parks a `RuntimeError` for the loop
// top is driven here, each 2000 times inside a try/catch so the throw, the
// handler walk and the loop's next bytecode run back to back:
//   * `iaload` / `laload` / `aaload` with an index past the end and a
//     negative one (the `*aload` arm's two parking sites),
//   * `iastore` / `lastore` past the end and negative (the `*astore` arm),
//   * `aastore` past the end of a covariant `String[]` seen as `Object[]`
//     (JVMS order: AIOOBE before the store check), an `Integer` and an
//     `int[]` stored into it (`ArrayStoreException` naming the value's
//     class, `[I` for the array),
// then a clean pass that must still read and write normally. The census of
// the pending-signal arm also covers the opcode-pair cursor, which the same
// wave boxed: run once with CRATONVM_QUICKEN_STATS=pairs too.
//
// Run (both lines must print the block below, with and without --nojit):
//   cratonvm --java-home <jdk25> --nojit -cp <dir> L7W84ParkedArrayErrors
//   CRATONVM_QUICKEN_STATS=pairs cratonvm --java-home <jdk25> --nojit -cp <dir> L7W84ParkedArrayErrors
// The expected lines are HotSpot's: `Index %d out of bounds for length %d`
// is `Universe`'s AIOOBE text (`SharedRuntime::throw_range_check_exception`
// / the template interpreter's `throw_ArrayIndexOutOfBoundsException`), and
// an aastore ArrayStoreException is the value's `external_name()`.
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// iaload past 2000 Index 5 out of bounds for length 3
// iaload neg 2000 Index -1 out of bounds for length 3
// laload past 2000 Index 2 out of bounds for length 2
// aaload neg 2000 Index -7 out of bounds for length 4
// iastore past 2000 Index 3 out of bounds for length 3
// iastore neg 2000 Index -2 out of bounds for length 3
// lastore past 2000 Index 9 out of bounds for length 2
// aastore past 2000 java.lang.ArrayIndexOutOfBoundsException Index 5 out of bounds for length 1
// aastore integer 2000 java.lang.ArrayStoreException java.lang.Integer
// aastore int-array 2000 java.lang.ArrayStoreException [I
// clean 6000 9 x
// HOTSPOT_EXPECTED_END
public class L7W84ParkedArrayErrors {
    static final int N = 2000;

    static int iaload(int[] a, int i) {
        return a[i];
    }

    static long laload(long[] a, int i) {
        return a[i];
    }

    static Object aaload(Object[] a, int i) {
        return a[i];
    }

    static void iastore(int[] a, int i, int v) {
        a[i] = v;
    }

    static void lastore(long[] a, int i, long v) {
        a[i] = v;
    }

    static void aastore(Object[] a, int i, Object v) {
        a[i] = v;
    }

    interface Op {
        void run();
    }

    static void count(String label, Op op) {
        int caught = 0;
        String first = null;
        for (int k = 0; k < N; k++) {
            try {
                op.run();
            } catch (ArrayIndexOutOfBoundsException e) {
                caught++;
                if (first == null) {
                    first = e.getMessage();
                }
            }
        }
        System.out.println(label + " " + caught + " " + first);
    }

    static void countAny(String label, Op op) {
        int caught = 0;
        String first = null;
        for (int k = 0; k < N; k++) {
            try {
                op.run();
            } catch (RuntimeException e) {
                caught++;
                if (first == null) {
                    first = e.getClass().getName() + " " + e.getMessage();
                }
            }
        }
        System.out.println(label + " " + caught + " " + first);
    }

    public static void main(String[] args) {
        int[] ints = {1, 2, 3};
        long[] longs = {4L, 5L};
        Object[] objs = {"a", "b", "c", "d"};
        Object[] strings = new String[1];

        count("iaload past", () -> iaload(ints, 5));
        count("iaload neg", () -> iaload(ints, -1));
        count("laload past", () -> laload(longs, 2));
        count("aaload neg", () -> aaload(objs, -7));
        count("iastore past", () -> iastore(ints, 3, 0));
        count("iastore neg", () -> iastore(ints, -2, 0));
        count("lastore past", () -> lastore(longs, 9, 0L));
        countAny("aastore past", () -> aastore(strings, 5, "s"));
        countAny("aastore integer", () -> aastore(strings, 0, Integer.valueOf(1)));
        countAny("aastore int-array", () -> aastore(strings, 0, new int[0]));

        // The loop still runs normally after 20000 parked errors.
        long sum = 0;
        for (int k = 0; k < N; k++) {
            iastore(ints, k % 3, k % 3 + 1);
            lastore(longs, k % 2, 2L);
            aastore(strings, 0, "x");
            sum += iaload(ints, k % 3) - (k % 3) + laload(longs, k % 2) - 2L;
        }
        System.out.println("clean " + (sum + 4000) + " " + (iaload(ints, 0) + iaload(ints, 1) + iaload(ints, 2) + laload(longs, 0) - 2 + laload(longs, 1) - 2 + 3) + " " + aaload(strings, 0));
    }
}
