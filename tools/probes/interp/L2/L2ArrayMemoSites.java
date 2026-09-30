// Interpreter round i1, wave 16, lane L4 — the array receiver memo at
// `checkcast` / `instanceof` sites (`CastSite::positive_array`).
//
// Compare stdout against HotSpot 25 (`java L2ArrayMemoSites`), with and
// without `--nojit`. Each row drives ONE bytecode site with a receiver the
// memo admitted first and then with receivers it must NOT answer: a different
// component, a primitive array, a nested array, a plain object whose class is
// the memoised component. A memo that answers by the wrong key shows up as a
// `true` / missing CCE where HotSpot prints `false` / CCE.
//
// Expected HotSpot 25 output:
//   r01 true true false false false
//   r02 ok ok CCE CCE CCE
//   r03 true true false true
//   r04 ok ok CCE ok
//   r05 true false false
//   r06 true true true false
//   r07 sum=300000 cce=100000
public class L2ArrayMemoSites {
    static final int WARM = 3;

    // One site: `instanceof [Ljava/lang/CharSequence;`.
    static boolean isCharSeqArray(Object o) {
        return o instanceof CharSequence[];
    }

    // One site: `checkcast [Ljava/lang/CharSequence;`.
    static String castCharSeqArray(Object o) {
        try {
            CharSequence[] a = (CharSequence[]) o;
            return a == null ? "null" : "ok";
        } catch (ClassCastException e) {
            return "CCE";
        }
    }

    // One site: `instanceof [Ljava/lang/Object;` (a trivial target).
    static boolean isObjectArray(Object o) {
        return o instanceof Object[];
    }

    // One site: `checkcast [Ljava/lang/Object;` — the erasure of `(T[]) x`.
    static String castObjectArray(Object o) {
        try {
            Object[] a = (Object[]) o;
            return a == null ? "null" : "ok";
        } catch (ClassCastException e) {
            return "CCE";
        }
    }

    // One site: `instanceof [I`.
    static boolean isIntArray(Object o) {
        return o instanceof int[];
    }

    // One site: `instanceof [[Ljava/lang/Number;`.
    static boolean isNumberMatrix(Object o) {
        return o instanceof Number[][];
    }

    @SuppressWarnings("unchecked")
    static <T> T[] erase(Object o) {
        return (T[]) o;
    }

    public static void main(String[] args) {
        String[] strings = { "a" };
        StringBuilder[] builders = { new StringBuilder() };
        Object[] objects = { "a" };
        Integer[] integers = { 1 };
        int[] ints = { 1 };
        String[][] stringMatrix = { { "a" } };
        int[][] intMatrix = { { 1 } };
        Integer[][] integerMatrix = { { 1 } };

        // r01: memo String[] at a CharSequence[] site, then another component
        // that IS a CharSequence (StringBuilder[]), then Object[], int[], and a
        // plain String (whose class id is the memoised component's).
        for (int i = 0; i < WARM; i++) isCharSeqArray(strings);
        System.out.println("r01 " + isCharSeqArray(strings) + " " + isCharSeqArray(builders)
                + " " + isCharSeqArray(objects) + " " + isCharSeqArray(ints)
                + " " + isCharSeqArray("a"));

        // r02: the checkcast twin.
        for (int i = 0; i < WARM; i++) castCharSeqArray(strings);
        System.out.println("r02 " + castCharSeqArray(strings) + " " + castCharSeqArray(builders)
                + " " + castCharSeqArray(objects) + " " + castCharSeqArray(ints)
                + " " + castCharSeqArray("a"));

        // r03: trivial Object[] target; a primitive array and a nested
        // primitive array (which IS an Object[]).
        for (int i = 0; i < WARM; i++) isObjectArray(strings);
        System.out.println("r03 " + isObjectArray(strings) + " " + isObjectArray(integers)
                + " " + isObjectArray(ints) + " " + isObjectArray(intMatrix));

        // r04: `(T[]) x` erasure: String[], Integer[], int[], a plain object,
        // int[][].
        for (int i = 0; i < WARM; i++) castObjectArray(strings);
        System.out.println("r04 " + castObjectArray(strings) + " " + castObjectArray(integers)
                + " " + castObjectArray(ints) + " " + castObjectArray(intMatrix));

        // r05: primitive target; long[] and Integer[] must not be answered by
        // the int[] memo.
        for (int i = 0; i < WARM; i++) isIntArray(ints);
        System.out.println("r05 " + isIntArray(ints) + " " + isIntArray(new long[1])
                + " " + isIntArray(integers));

        // r06: nested reference target: Integer[][] memoised, then
        // Number[][] itself, Integer[][] again, String[][] refused.
        for (int i = 0; i < WARM; i++) isNumberMatrix(integerMatrix);
        System.out.println("r06 " + isNumberMatrix(integerMatrix)
                + " " + isNumberMatrix(new Number[1][1]) + " " + isNumberMatrix(integerMatrix)
                + " " + isNumberMatrix(stringMatrix));

        // r07: a rotating erasure site: String[], Integer[], int[] in turn.
        Object[] rotation = { strings, integers, ints };
        int sum = 0;
        int cce = 0;
        for (int i = 0; i < 300_000; i++) {
            try {
                Object[] a = L2ArrayMemoSites.<Object>erase(rotation[i % 3]);
                sum += a.length;
            } catch (ClassCastException e) {
                cce++;
            }
        }
        System.out.println("r07 sum=" + (sum + cce) + " cce=" + cce);
    }
}
