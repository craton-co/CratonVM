import java.nio.CharBuffer;
import java.util.*;
import java.util.function.Supplier;

/**
 * nio20 wave 1, lane L3: NIO19-5 (`getClass()` as two loads in the optimizing tier) and
 * NIO19-8 (`HeapCharBuffer.put(String,int,int)` on the optimizing tier).
 *
 * Bench rows print `<row> <N> ns/op`; conformance rows print deterministic facts to diff
 * against HotSpot 25. `-Dnio20.rows=a,b` selects rows (default: all). `-Dnio20.ops=N` sets the
 * bench op count (default 20_000_000).
 *
 *   getclass   Object-typed getClass() on a plain object, in a hot non-inlined method
 *   getclassmix  getClass() over a mixed receiver set (plain, remapped collection stamps, arrays)
 *   getclasseq   getClass() == Plain.class only (wave 4: `getclass` also pays Class.hashCode())
 *   classhash    Class.hashCode() only (an identity-hash native call; no getClass())
 *   idhash       System.identityHashCode(plain object) only (wave 5: the inline header read)
 *   objclasshash Object-typed hashCode() of a Class receiver (HashMap<Class<?>,V>.hash's site)
 *   getclasslambda  getClass() of a lambda == its class (wave 5: the lambda-proxy word band)
 *   getclasslit  (wave 6) R17NioRows' getclass shape: getClass() of 8 mixed receivers in the
 *              loop itself, compared with up to four class literals; ns per getClass
 *   conf       also (wave 5) identity-hash facts: stable across hot loops, locking, GC
 *   heapcb     HeapCharBuffer.put(String,int,int)
 *   conf       getClass() facts: identity with class literals across System.gc(), remapped
 *              collection display names, arrays, lambdas, null-receiver NPE message
 */
public class Nio20GetClass {
    static String rows = System.getProperty("nio20.rows", "all");
    static long ops = Long.getLong("nio20.ops", 20_000_000L);
    static boolean on(String r) { return rows.equals("all") || Arrays.asList(rows.split(",")).contains(r); }

    static final class Plain { int x; }
    static volatile Object sink;

    // Kept out of line so it tiers up as its own method (method-entry door, optimizing tier).
    static Class<?> cls(Object o) { return o.getClass(); }

    static long getclassLoop(Object o, long n) {
        long acc = 0;
        for (long i = 0; i < n; i++) {
            acc += cls(o).hashCode() & 1;
        }
        return acc;
    }

    // nio20 wave 4, lane L2: `getclass` above is getClass() PLUS Class.hashCode(), an
    // identity-hash native call (HotSpot inlines it to a header load); these two rows split
    // the cost so the getClass half can be read on its own.
    //   getclasseq   getClass() compared with a class literal, no hash
    //   classhash    Class.hashCode() of a class held in a static, no getClass()
    static Class<?> held = Plain.class;

    static long getclassEqLoop(Object o, long n) {
        long acc = 0;
        for (long i = 0; i < n; i++) {
            if (cls(o) == Plain.class) acc++;
        }
        return acc;
    }

    static long classHashLoop(long n) {
        long acc = 0;
        for (long i = 0; i < n; i++) {
            acc += held.hashCode() & 1;
        }
        return acc;
    }

    // nio20 wave 5, lane L2: the identity hash alone, and getClass() of a lambda alone.
    static long idHashLoop(Object o, long n) {
        long acc = 0;
        for (long i = 0; i < n; i++) {
            acc += System.identityHashCode(o) & 1;
        }
        return acc;
    }

    static long objClassHashLoop(Object o, long n) {
        long acc = 0;
        for (long i = 0; i < n; i++) {
            acc += o.hashCode() & 1;
        }
        return acc;
    }

    static long lambdaClassLoop(Object o, Class<?> c, long n) {
        long acc = 0;
        for (long i = 0; i < n; i++) {
            if (cls(o) == c) acc++;
        }
        return acc;
    }

    static long mixLoop(Object[] os, long n) {
        long acc = 0;
        for (long i = 0; i < n; i++) {
            acc += System.identityHashCode(cls(os[(int) (i & 7)])) & 1;
        }
        return acc;
    }

    // nio20 wave 6, lane L3: the R17NioRows `getclass` shape -- getClass() straight in the loop
    // (no `cls` helper) on an `Object[]` element, each answer compared with up to four class
    // literals. Every `ldc <Class>` was a helper CALL per execution in a body the background
    // worker compiled before any mutator minted the site's slot
    // (CRATONVM_JIT_LDC_SLOT_MINT_AT_OFFER=0 restores that).
    static final Object[] LIT_RECEIVERS = {new Plain(), "s", Integer.valueOf(7), new StringBuilder(),
            new Object(), new Plain(), "t", Integer.valueOf(9)};

    static long getclassLitLoop(long n) {
        long acc = 0;
        Object[] os = LIT_RECEIVERS;
        for (long i = 0; i < n; i++) {
            for (int k = 0; k < 16; k++) {
                Class<?> c = os[(int) ((i + k) & 7)].getClass();
                if (c == String.class) acc += 1;
                else if (c == Integer.class) acc += 2;
                else if (c == StringBuilder.class) acc += 3;
                else if (c == Object.class) acc += 4;
            }
        }
        return acc;
    }

    static long heapcbLoop(long n) {
        CharBuffer cb = CharBuffer.allocate(64);
        String s = "abcdefghijklmnop";
        long acc = 0;
        for (long i = 0; i < n; i++) {
            cb.clear();
            cb.put(s, 0, 16);
            acc += cb.position();
        }
        return acc;
    }

    static void bench(String row, Supplier<Long> body) {
        // Warm, then time.
        sink = body.get();
        long t0 = System.nanoTime();
        sink = body.get();
        long t1 = System.nanoTime();
        System.out.println(row + " " + ((t1 - t0) / Math.max(1, ops)) + " ns/op");
    }

    public static void main(String[] a) {
        if (on("conf")) conf();
        if (on("getclass")) bench("getclass", () -> getclassLoop(new Plain(), ops));
        if (on("getclasseq")) bench("getclasseq", () -> getclassEqLoop(new Plain(), ops));
        if (on("classhash")) bench("classhash", () -> classHashLoop(ops));
        if (on("idhash")) {
            Object o = new Plain();
            bench("idhash", () -> idHashLoop(o, ops));
        }
        if (on("objclasshash")) bench("objclasshash", () -> objClassHashLoop(Plain.class, ops));
        if (on("getclasslambda")) {
            Runnable r = () -> { };
            Class<?> c = r.getClass();
            bench("getclasslambda", () -> lambdaClassLoop(r, c, ops));
        }
        if (on("getclassmix")) {
            Object[] os = {new Plain(), List.of(1, 2, 3), Collections.unmodifiableList(new ArrayList<>()),
                    new int[1], "s", new String[0], Map.of(1, 2), (Runnable) () -> { }};
            bench("getclassmix", () -> mixLoop(os, ops));
        }
        if (on("getclasslit")) bench("getclasslit", () -> getclassLitLoop(ops / 16));
        if (on("heapcb")) bench("heapcb", () -> heapcbLoop(ops / 10));
        System.out.println("done");
    }

    static String name(Object o) { return cls(o).getName(); }

    // nio20 wave 5, lane L2: identity hashes must not change under the inline read --
    // not across a hot loop (compiled), not while or after the object is locked (a compact
    // instance's hash then lives in its monitor), not across a collection, and
    // Object.hashCode() of a non-overriding object (and of a Class) is the same value.
    static int idh(Object o) { return System.identityHashCode(o); }
    static int oh(Object o) { return o.hashCode(); }

    static void idHashConf() {
        Object[] objs = {new Plain(), new Object(), new int[3], new String[2], Plain.class, String.class,
                new Plain(), new Object()};
        int[] first = new int[objs.length];
        for (int k = 0; k < objs.length; k++) first[k] = idh(objs[k]);
        boolean stable = true, objEq = true, nonZero = true;
        for (int round = 0; round < 3; round++) {
            for (int i = 0; i < 300_000; i++) {
                int k = i % objs.length;
                stable &= idh(objs[k]) == first[k];
                if (!(objs[k] instanceof String)) objEq &= oh(objs[k]) == first[k];
                nonZero &= first[k] != 0;
            }
            synchronized (objs[0]) {
                for (int i = 0; i < 100_000; i++) stable &= idh(objs[0]) == first[0];
            }
            System.gc();
        }
        // An object hashed for the first time WHILE locked, then read hot after unlock.
        Object late = new Plain();
        int lh;
        synchronized (late) { lh = idh(late); }
        boolean lateStable = true;
        for (int i = 0; i < 300_000; i++) lateStable &= idh(late) == lh && oh(late) == lh;
        System.gc();
        lateStable &= idh(late) == lh;
        System.out.println("conf idhash stable=" + stable + " objectHashCode=" + objEq + " nonzero=" + nonZero
                + " lockedFirst=" + lateStable + " null=" + idh(null));
    }

    static void conf() {
        Object plain = new Plain();
        Object[] remapped = {List.of(), List.of(1), List.of(1, 2, 3), Set.of(1), Set.of(1, 2, 3), Map.of(),
                Map.of(1, 2), Collections.unmodifiableList(new ArrayList<>()),
                Collections.unmodifiableList(new LinkedList<>()), Collections.unmodifiableSet(new HashSet<>()),
                Collections.unmodifiableMap(new HashMap<>()), new ArrayList<>().iterator(),
                Map.of(1, 2).keySet(), Map.of(1, 2).entrySet()};
        Object[] arrays = {new int[0], new String[0], new Plain[0], new int[0][0], new Object[0]};
        // Enough calls for `cls` to reach the optimizing tier before and after the collections.
        for (int round = 0; round < 4; round++) {
            boolean same = true;
            for (int i = 0; i < 200_000; i++) {
                same &= cls(plain) == Plain.class;
                same &= cls("x") == String.class;
                same &= cls(remapped[i % remapped.length]) != null;
                same &= cls(arrays[i % arrays.length]).isArray();
            }
            System.out.println("conf round" + round + " identity=" + same);
            System.gc();
        }
        for (Object o : remapped) System.out.println("conf remapped " + name(o));
        for (Object o : arrays) System.out.println("conf array " + name(o));
        Runnable r = () -> { };
        System.out.println("conf lambda hidden=" + cls(r).isHidden() + " synthetic=" + cls(r).isSynthetic());
        // nio20 wave 5: a lambda's getClass() stays its own class across hot calls and GC.
        Class<?> rc = r.getClass();
        boolean lsame = true;
        for (int round = 0; round < 3; round++) {
            for (int i = 0; i < 200_000; i++) lsame &= cls(r) == rc;
            System.gc();
        }
        System.out.println("conf lambda stable=" + lsame);
        idHashConf();
        System.out.println("conf literal " + (cls(new Plain()) == Plain.class));
        // nio20 wave 6: the literal-compare loop's answer, warm (compiled) and cold.
        long lit = 0;
        for (int k = 0; k < 40; k++) lit = getclassLitLoop(5_000);
        System.out.println("conf getclasslit " + lit + " " + getclassLitLoop(3));
        try {
            Object nul = null;
            sink = nul.getClass();
            System.out.println("conf npe none");
        } catch (NullPointerException e) {
            System.out.println("conf npe " + e.getMessage());
        }
        try {
            sink = cls(null);
            System.out.println("conf npe2 none");
        } catch (NullPointerException e) {
            System.out.println("conf npe2 " + e.getMessage());
        }
    }
}
