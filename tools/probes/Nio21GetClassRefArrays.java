import java.io.InputStream;
import java.lang.reflect.Array;
import java.util.Arrays;

/**
 * nio21 wave 2, lane L5: `getClass()` of a REFERENCE array answered from the compiled-`getClass()`
 * table's reference-array band (`CRATONVM_JIT_GETCLASS_REF_ARRAY_WORDS`, keyed by the header's
 * component id; page `r20-getclass-array-receivers-have-no-word-FIXED-20261008.md`).
 *
 * Conformance (default, or `-Dnio21.rows=conf`) prints answers only, to diff against HotSpot 25 in
 * both modes and with the switch `=0`. It covers the arms the band must NOT answer differently
 * from the native: an array over a class a child-first loader defines under the SAME name as an
 * application class (before and after that loader exists, so the application array's name turns
 * ambiguous mid-run), arrays over interfaces, enums, nested arrays, a lambda's array, and locked /
 * hashed arrays, each hot in both tiers and across collections that move the mirrors.
 *
 * Bench rows (`-Dnio21.rows=strarr,objarr,mixref`) print `<row> <N> ns/op`; `-Dnio21.ops=N` sets
 * the op count (default 20_000_000).
 *
 *   strarr   getClass() of one String[]
 *   objarr   getClass() of one Object[]
 *   mixref   String[], Object[], Item[], int[][] rotating
 */
public class Nio21GetClassRefArrays {
    static String rows = System.getProperty("nio21.rows", "conf");
    static long ops = Long.getLong("nio21.ops", 20_000_000L);
    static boolean on(String r) { return Arrays.asList(rows.split(",")).contains(r); }

    public static class Item {}
    enum Color { RED }
    interface Shape {}

    // Out of line so it tiers up on its own (method-entry door, both tiers).
    static Class<?> cls(Object o) { return o.getClass(); }

    /** Child-first for `Item` only: defines its own `Nio21GetClassRefArrays$Item`. */
    static final class ChildFirst extends ClassLoader {
        ChildFirst(ClassLoader parent) { super(parent); }
        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (!name.equals(Item.class.getName())) return super.loadClass(name, resolve);
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c == null) {
                    String res = name.replace('.', '/') + ".class";
                    try (InputStream in = Nio21GetClassRefArrays.class.getClassLoader().getResourceAsStream(res)) {
                        byte[] b = in.readAllBytes();
                        c = defineClass(name, b, 0, b.length);
                    } catch (java.io.IOException e) {
                        throw new ClassNotFoundException(name, e);
                    }
                }
                return c;
            }
        }
    }

    static boolean hot(Object[] arrays, Class<?>[] classes, int n) {
        boolean same = true;
        for (int i = 0; i < n; i++) {
            int k = i % arrays.length;
            same &= cls(arrays[k]) == classes[k];
        }
        return same;
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

    public static void main(String[] args) throws Exception {
        if (on("conf")) conf();
        if (on("strarr")) bench("strarr", () -> oneLoop(new String[3], String[].class, ops));
        if (on("objarr")) bench("objarr", () -> oneLoop(new Object[3], Object[].class, ops));
        if (on("mixref")) {
            Object[] os = {new String[1], new Object[1], new Item[1], new int[1][1]};
            bench("mixref", () -> mixLoop(os, ops));
        }
    }

    static void conf() throws Exception {
        Runnable lambda = () -> {};
        Object lambdaArray = Array.newInstance(lambda.getClass(), 1);
        Object[] arrays = {new String[0], new Object[1], new Item[2], new Color[0], new Shape[1],
                new int[0][0], new String[1][1], new Item[1][1], new Runnable[0], lambdaArray};
        Class<?>[] classes = {String[].class, Object[].class, Item[].class, Color[].class, Shape[].class,
                int[][].class, String[][].class, Item[][].class, Runnable[].class, lambda.getClass().arrayType()};
        // Phase 1: hot before any user loader exists.
        for (int round = 0; round < 3; round++) {
            System.out.println("conf before round" + round + " identity=" + hot(arrays, classes, 400_000));
            System.gc();
        }
        // A child-first loader defines its own `Item`: `[L...$Item;` now names two classes.
        ClassLoader child = new ChildFirst(Nio21GetClassRefArrays.class.getClassLoader());
        Class<?> childItem = Class.forName(Item.class.getName(), true, child);
        Object childArray = Array.newInstance(childItem, 2);
        Object childArray2 = Array.newInstance(childItem.arrayType(), 1);
        System.out.println("conf child distinct=" + (childItem != Item.class)
                + " sameName=" + childItem.getName().equals(Item.class.getName()));
        Object[] arrays2 = Arrays.copyOf(arrays, arrays.length + 2);
        Class<?>[] classes2 = Arrays.copyOf(classes, classes.length + 2);
        arrays2[arrays.length] = childArray;
        classes2[arrays.length] = childItem.arrayType();
        arrays2[arrays.length + 1] = childArray2;
        classes2[arrays.length + 1] = childItem.arrayType().arrayType();
        // Phase 2: hot after; the application's `Item[]` must still answer `Item[].class`.
        for (int round = 0; round < 3; round++) {
            System.out.println("conf after round" + round + " identity=" + hot(arrays2, classes2, 400_000));
            System.gc();
        }
        for (Object o : arrays2) {
            Class<?> c = cls(o);
            Class<?> comp = c.getComponentType();
            String name = c.getName().contains("$$Lambda") ? "<lambda>[]" : c.getName();
            System.out.println("conf ref " + name + " array=" + c.isArray()
                    + " primitive=" + c.isPrimitive()
                    + " componentLoaderIsChild=" + (comp.getClassLoader() == child)
                    + " arrayTypeRoundTrip=" + (comp.arrayType() == c)
                    + " newInstanceSame=" + (Array.newInstance(comp, 0).getClass() == c)
                    + " hashStable=" + (c.hashCode() == cls(o).hashCode()));
        }
        System.out.println("conf appItemVsChild " + (cls(new Item[0]) == cls(childArray))
                + " " + (cls(new Item[0]) == Item[].class)
                + " " + (cls(childArray) == childItem.arrayType()));
        // A locked and a hashed reference array keep their class.
        String[] locked = new String[2];
        boolean lockedSame;
        synchronized (locked) { lockedSame = cls(locked) == String[].class; }
        Object[] hashed = new Object[2];
        int h = System.identityHashCode(hashed);
        System.out.println("conf locked " + lockedSame + " hashed " + (cls(hashed) == Object[].class)
                + " hashKept " + (h == System.identityHashCode(hashed)));
        try {
            cls(null);
            System.out.println("conf npe none");
        } catch (NullPointerException e) {
            System.out.println("conf npe " + e.getMessage());
        }
    }
}
