import java.util.Arrays;

/**
 * nio21 wave 1, lane L5: `getClass()` of a one-dimensional PRIMITIVE array answered from the
 * compiled-`getClass()` table's primitive-array band (`CRATONVM_JIT_GETCLASS_PRIM_ARRAY_WORDS`,
 * page `r20-getclass-array-receivers-have-no-word-FIXED-20261008.md`).
 *
 * Conformance (default, or `-Dnio21.rows=conf`) prints answers only, to diff against HotSpot 25.
 * Bench rows (`-Dnio21.rows=primarr,intarr,refarr,mixarr`) print `<row> <N> ns/op`;
 * `-Dnio21.ops=N` sets the op count (default 20_000_000).
 *
 *   primarr  getClass() over the eight primitive array types, rotating
 *   intarr   getClass() of one int[] (the common receiver)
 *   refarr   getClass() of a String[] (control: reference arrays keep the helper)
 *   mixarr   Nio20GetClass getclassmix's array share: int[], String[], byte[], char[]
 */
public class Nio21GetClassArrays {
    static String rows = System.getProperty("nio21.rows", "conf");
    static long ops = Long.getLong("nio21.ops", 20_000_000L);
    static boolean on(String r) { return Arrays.asList(rows.split(",")).contains(r); }

    // Out of line so it tiers up on its own (method-entry door, both tiers).
    static Class<?> cls(Object o) { return o.getClass(); }

    static final Object[] PRIMS = {new boolean[1], new char[2], new float[3], new double[4],
            new byte[5], new short[6], new int[7], new long[8]};
    static final Class<?>[] PRIM_CLASSES = {boolean[].class, char[].class, float[].class, double[].class,
            byte[].class, short[].class, int[].class, long[].class};

    static long primLoop(long n) {
        long acc = 0;
        Object[] os = PRIMS;
        for (long i = 0; i < n; i++) {
            int k = (int) (i & 7);
            if (cls(os[k]) == PRIM_CLASSES[k]) acc++;
        }
        return acc;
    }

    static long oneLoop(Object o, Class<?> c, long n) {
        long acc = 0;
        for (long i = 0; i < n; i++) {
            if (cls(o) == c) acc++;
        }
        return acc;
    }

    static long mixLoop(Object[] os, long n) {
        long acc = 0;
        for (long i = 0; i < n; i++) {
            acc += cls(os[(int) (i & 3)]).getName().length();
        }
        return acc;
    }

    static void bench(String row, java.util.function.LongSupplier body) {
        body.getAsLong(); // warm-up
        long t0 = System.nanoTime();
        long r = body.getAsLong();
        long t1 = System.nanoTime();
        if (r == Long.MIN_VALUE) System.out.println("impossible");
        System.out.println(row + " " + ((t1 - t0) / ops) + " ns/op");
    }

    public static void main(String[] args) {
        if (on("conf")) conf();
        if (on("primarr")) bench("primarr", () -> primLoop(ops));
        if (on("intarr")) bench("intarr", () -> oneLoop(new int[3], int[].class, ops));
        if (on("refarr")) bench("refarr", () -> oneLoop(new String[3], String[].class, ops));
        if (on("mixarr")) {
            Object[] os = {new int[1], new String[1], new byte[1], new char[1]};
            bench("mixarr", () -> mixLoop(os, ops));
        }
    }

    static void conf() {
        Object[] others = {new int[0][0], new String[0], new Object[0], new long[1][1][1], new Runnable[0]};
        Class<?>[] otherClasses = {int[][].class, String[].class, Object[].class, long[][][].class, Runnable[].class};
        // Enough calls for `cls` to reach both tiers, around collections that move the mirrors.
        for (int round = 0; round < 4; round++) {
            boolean same = true;
            for (int i = 0; i < 300_000; i++) {
                int k = i & 7;
                same &= cls(PRIMS[k]) == PRIM_CLASSES[k];
                same &= cls(new int[i & 3]) == int[].class;
                same &= cls(others[i % others.length]) == otherClasses[i % others.length];
                same &= cls("s") == String.class;
            }
            System.out.println("conf round" + round + " identity=" + same);
            System.gc();
        }
        for (Object o : PRIMS) {
            Class<?> c = cls(o);
            System.out.println("conf prim " + c.getName() + " component=" + c.getComponentType()
                    + " primitive=" + c.isPrimitive() + " array=" + c.isArray()
                    + " hashStable=" + (c.hashCode() == cls(o).hashCode()));
        }
        for (Object o : others) System.out.println("conf other " + cls(o).getName());
        // A locked array and a hashed array keep their class.
        int[] locked = new int[2];
        boolean lockedSame;
        synchronized (locked) { lockedSame = cls(locked) == int[].class; }
        int[] hashed = new int[2];
        int h = System.identityHashCode(hashed);
        System.out.println("conf locked " + lockedSame + " hashed " + (cls(hashed) == int[].class)
                + " hashKept " + (h == System.identityHashCode(hashed)));
        try {
            cls(null);
            System.out.println("conf npe none");
        } catch (NullPointerException e) {
            System.out.println("conf npe " + e.getMessage());
        }
    }
}
