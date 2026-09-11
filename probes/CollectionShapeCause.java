import java.lang.reflect.Array;
import java.lang.reflect.Field;
import java.lang.reflect.Modifier;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.HashSet;
import java.util.IdentityHashMap;
import java.util.LinkedHashMap;
import java.util.Locale;
import java.util.TreeMap;
import java.util.TreeSet;
import java.util.function.Supplier;

/**
 * WHY a collection instance is wide, not just THAT it is.
 *
 * `AllocShapeTruth` reads retained heap per instance, which is the right
 * number and cannot distinguish its two causes: an object with too many SLOTS,
 * and an object of the right width that eagerly RETAINS an array HotSpot
 * leaves null. `docs/internal/fixed-bugs/jdk-collection-classes-are-padded-to-
 * a-synthetic-stub-floor-FIXED-20260911.md` was opened attributing both of its
 * TreeMap (352 B) and IdentityHashMap (584 B) rows to a field-count ratchet;
 * this probe is what showed neither was one -- the first is an eagerly
 * allocated backing array, and the second is an array HOTSPOT allocates too,
 * at 8-byte references instead of 4.
 *
 * This probe prints, per shape, the retained bytes AND every declared instance
 * field with its live value, so a non-null array on a fresh instance is
 * visible next to the number it explains. Run it against HotSpot and CratonVM
 * and diff.
 */
public class CollectionShapeCause {

    static Object[] keep;

    public static void main(String[] args) throws Exception {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 50000;

        System.out.println(String.format("%-28s %10s", "shape (empty)", "retained"));
        row("ArrayList", n, ArrayList::new);
        row("HashMap", n, HashMap::new);
        row("LinkedHashMap", n, LinkedHashMap::new);
        row("HashSet", n, HashSet::new);
        row("TreeMap", n, TreeMap::new);
        row("TreeSet", n, TreeSet::new);
        row("IdentityHashMap", n, IdentityHashMap::new);
        System.out.println();

        fields("java.util.ArrayList", new ArrayList<>());
        fields("java.util.HashMap", new HashMap<>());
        fields("java.util.LinkedHashMap", new LinkedHashMap<>());
        fields("java.util.HashSet", new HashSet<>());
        fields("java.util.TreeMap", new TreeMap<>());
        fields("java.util.TreeSet", new TreeSet<>());
        fields("java.util.IdentityHashMap", new IdentityHashMap<>());
        System.out.println("CAUSE_END");
    }

    static void row(String name, int n, Supplier<Object> make) throws Exception {
        for (int i = 0; i < 2000; i++) make.get();
        Runtime rt = Runtime.getRuntime();
        Object[] hold = new Object[n];
        keep = hold;
        settle(rt);
        long h0 = rt.totalMemory() - rt.freeMemory();
        for (int i = 0; i < n; i++) hold[i] = make.get();
        settle(rt);
        long retained = (rt.totalMemory() - rt.freeMemory()) - h0;
        if (keep.length != n) throw new IllegalStateException("unreachable");
        System.out.println(String.format(Locale.ROOT, "%-28s %10.1f", name, retained / (double) n));
        keep = null;
    }

    static void fields(String label, Object o) {
        System.out.println("== " + label);
        int slots = 0;
        for (Class<?> c = o.getClass(); c != null && c != Object.class; c = c.getSuperclass()) {
            for (Field f : c.getDeclaredFields()) {
                if (Modifier.isStatic(f.getModifiers())) {
                    continue;
                }
                slots++;
                String v;
                try {
                    f.setAccessible(true);
                    v = describe(f.get(o));
                } catch (Throwable t) {
                    v = "<" + t.getClass().getSimpleName() + ">";
                }
                System.out.println(String.format("   %-34s %-20s %s",
                        c.getSimpleName() + "." + f.getName(), f.getType().getSimpleName(), v));
            }
        }
        System.out.println("   declared instance fields: " + slots);
    }

    static String describe(Object x) {
        if (x == null) {
            return "null";
        }
        if (x.getClass().isArray()) {
            return x.getClass().getSimpleName() + " len=" + Array.getLength(x);
        }
        return x.getClass().getSimpleName() + " " + x;
    }

    static void settle(Runtime rt) throws Exception {
        for (int i = 0; i < 3; i++) {
            System.gc();
            Thread.sleep(40);
        }
    }
}
