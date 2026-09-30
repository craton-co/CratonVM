/*
 * Interpreter round i1, wave 22, lane L7: the quickened `aastore`
 * (`field_fast::array_store_ref`, tried first by the dispatch loop's 0x53
 * arm).
 *
 * Before wave 22 every interpreted reference-array store went through the
 * general arm: two operand decodes, `array_length` / `kind_of` /
 * `element_type_of` through the `VmHeap` enum dispatch, the SATB read of the
 * old element through `get_array_element`, and `set_array_element` (header,
 * bounds, corpse audit and a `Value` match again). The quickened arm reads the
 * header once, confirms assignability with the two lock-free verdicts the
 * general arm already asked first (the `Object[]` component memo, the
 * published supers closure), and stores through the raw slot with the same
 * SATB and card barriers. Anything else declines to the general arm.
 *
 * Run: CratonVM `--nojit` (the interpreter's `aastore`; with the JIT the loops
 * compile), default mode and `--compatible`. A/B against the previous build,
 * interleaved; timings (ns per store, median of 5 rounds) go to stderr:
 *
 *   object[]  <- String         Object[] component: the memo answers
 *   string[]  <- String         exact component: the published closure
 *   number[]  <- Integer        superclass component: the published closure
 *   compar[]  <- String         interface component: the published closure
 *   null      -> Object[]       null element: no covariance question
 *
 * Expected direction: every row faster (the general arm cost ~2.2x a null
 * store before the `Object[]` memo; the rows here are what remains of the
 * whole arm). `CRATONVM_DBG_FIELD_SITE=1` prints `aastore: hit=… miss=…`:
 * `hit` must carry the timed loops (about 5 x 5 x N), `miss` only the
 * exception rows below.
 *
 * stdout is deterministic and identical on HotSpot 25 (`-Xint` or not):
 *
 *   ase java.lang.ArrayStoreException: java.lang.Integer
 *   ase-iface java.lang.ArrayStoreException: java.lang.Object
 *   ase-array java.lang.ArrayStoreException: [I
 *   aioobe java.lang.ArrayIndexOutOfBoundsException: Index 3 out of bounds for length 3
 *   aioobe-neg java.lang.ArrayIndexOutOfBoundsException: Index -1 out of bounds for length 3
 *   npe java.lang.NullPointerException
 *   ok-array [I [[I java.lang.String
 *   ok-null true
 *   checksum=1000000
 */
public class L7W22AastoreBench {
    static final int N = 200_000;

    static Object[] objs = new Object[64];
    static String[] strs = new String[64];
    static Number[] nums = new Number[64];
    @SuppressWarnings("rawtypes")
    static Comparable[] comps = new Comparable[64];

    static long objectRound(String s) {
        Object[] a = objs;
        for (int i = 0; i < N; i++) {
            a[i & 63] = s;
        }
        return a[7] == s ? N : 0;
    }

    static long stringRound(String s) {
        String[] a = strs;
        for (int i = 0; i < N; i++) {
            a[i & 63] = s;
        }
        return a[7] == s ? N : 0;
    }

    static long numberRound(Integer v) {
        Number[] a = nums;
        for (int i = 0; i < N; i++) {
            a[i & 63] = v;
        }
        return a[7] == v ? N : 0;
    }

    static long comparableRound(String s) {
        @SuppressWarnings("rawtypes")
        Comparable[] a = comps;
        for (int i = 0; i < N; i++) {
            a[i & 63] = s;
        }
        return a[7] == s ? N : 0;
    }

    static long nullRound() {
        Object[] a = objs;
        for (int i = 0; i < N; i++) {
            a[i & 63] = null;
        }
        return a[7] == null ? N : 0;
    }

    interface Round {
        long run();
    }

    static long timed(String label, Round r) {
        long[] ns = new long[5];
        long sum = 0;
        for (int k = 0; k < 5; k++) {
            long t0 = System.nanoTime();
            sum += r.run();
            ns[k] = System.nanoTime() - t0;
        }
        java.util.Arrays.sort(ns);
        System.err.printf(java.util.Locale.ROOT, "%-10s %6.1f ns/store%n", label, ns[2] / (double) N);
        return sum / 5;
    }

    static String name(Throwable t) {
        return t.getClass().getName() + ": " + t.getMessage();
    }

    static void exceptionRows() {
        Object[] aliased = new String[3];
        try {
            aliased[0] = Integer.valueOf(1);
            System.out.println("ase none");
        } catch (ArrayStoreException e) {
            System.out.println("ase " + name(e));
        }
        @SuppressWarnings("rawtypes")
        Object[] ifaceAliased = new Comparable[3];
        try {
            ifaceAliased[1] = new Object();
            System.out.println("ase-iface none");
        } catch (ArrayStoreException e) {
            System.out.println("ase-iface " + name(e));
        }
        Object[] arrAliased = new String[2][];
        try {
            arrAliased[0] = new int[1];
            System.out.println("ase-array none");
        } catch (ArrayStoreException e) {
            System.out.println("ase-array " + name(e));
        }
        Object[] three = new Object[3];
        int idx = 3;
        try {
            three[idx] = "x";
            System.out.println("aioobe none");
        } catch (ArrayIndexOutOfBoundsException e) {
            System.out.println("aioobe " + name(e));
        }
        idx = -1;
        try {
            three[idx] = "x";
            System.out.println("aioobe-neg none");
        } catch (ArrayIndexOutOfBoundsException e) {
            System.out.println("aioobe-neg " + name(e));
        }
        Object[] none = null;
        try {
            none[0] = "x";
            System.out.println("npe none");
        } catch (NullPointerException e) {
            // Class only: the helpful-NPE text depends on -g.
            System.out.println("npe " + e.getClass().getName());
        }
        Object[] mixed = new Object[3];
        mixed[0] = new int[2];
        mixed[1] = new int[1][];
        mixed[2] = "s";
        Object[][] nested = new Object[1][];
        nested[0] = new String[1];
        System.out.println("ok-array " + mixed[0].getClass().getName() + " "
                + mixed[1].getClass().getName() + " " + mixed[2].getClass().getName());
        strs[5] = "y";
        strs[5] = null;
        System.out.println("ok-null " + (strs[5] == null && nested[0].length == 1));
    }

    public static void main(String[] args) {
        exceptionRows();
        long sum = 0;
        sum += timed("object[]", () -> objectRound("a"));
        sum += timed("string[]", () -> stringRound("b"));
        sum += timed("number[]", () -> numberRound(Integer.valueOf(100_000)));
        sum += timed("compar[]", () -> comparableRound("c"));
        sum += timed("null", L7W22AastoreBench::nullRound);
        // Every row returns N per round (5 rows).
        System.out.println("checksum=" + sum);
    }
}
