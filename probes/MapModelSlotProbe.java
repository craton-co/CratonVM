import java.lang.reflect.*;
import java.util.*;

/**
 * Does a `java.util.HashMap` built by this VM carry the fields the image
 * declares, or CratonVM's fabricated `(buckets@0, size@1, capacity@2)` model
 * written on top of them?
 *
 * The overlay census cannot answer this. Slot 0 on a real `HashMap` is
 * `AbstractMap.keySet`, a REFERENCE, and the bucket array we store there is
 * also a reference — a same-kind wrong-field write, which
 * `overlay_access_is_cross_type` is blind to by construction. The
 * shadow-layout diff sees the model/real disagreement but cannot say what the
 * slot actually holds at runtime.
 *
 * Reflection can. `AbstractMap.keySet` and `AbstractMap.values` are declared
 * fields; reading them tells us exactly what is in them. Requires
 *   --add-opens java.base/java.util=ALL-UNNAMED
 * and the probe says so rather than silently reporting "inaccessible" as if it
 * were "clean" — a probe that cannot see the defect must not print a pass.
 *
 * Expected on HotSpot: both fields are null until the corresponding view is
 * asked for, then hold a `HashMap$KeySet` / `HashMap$Values`. Never an array,
 * never an Integer.
 */
public class MapModelSlotProbe {

    public static void main(String[] args) {
        Field keySet = declared("keySet");
        Field values = declared("values");
        Field table = declared2("table");
        Field size = declared2("size");
        if (keySet == null || values == null) {
            // The one outcome that must never read as a pass.
            System.out.println("MAPMODEL UNAVAILABLE — rerun with "
                    + "--add-opens java.base/java.util=ALL-UNNAMED");
            System.out.println("MAPMODEL done sections=0");
            return;
        }

        Map<String, Integer> fresh = new HashMap<>();
        report("fresh", fresh, keySet, values, table, size);

        Map<String, Integer> filled = new HashMap<>();
        for (int i = 0; i < 40; i++) {
            filled.put("k" + i, i);
        }
        report("filled", filled, keySet, values, table, size);

        // After the views are materialised the JDK caches them in these very
        // fields — the paired half: the fields are not permanently null, they
        // are null until used and then hold a view of the right TYPE.
        filled.keySet().size();
        filled.values().size();
        report("afterViews", filled, keySet, values, table, size);

        Map<String, Integer> copied = new HashMap<>(filled);
        report("copyCtor", copied, keySet, values, table, size);

        Map<String, Integer> sized = new HashMap<>(64);
        sized.put("a", 1);
        report("sizedCtor", sized, keySet, values, table, size);

        Map<String, Integer> linked = new LinkedHashMap<>();
        linked.put("a", 1);
        report("linked", linked, keySet, values, table, size);

        Set<String> hashSet = new HashSet<>(List.of("x", "y"));
        System.out.println("hashSetBacking " + backingOf(hashSet, keySet, values, table, size));

        System.out.println("MAPMODEL done sections=7");
    }

    /** `AbstractMap`'s two cached-view fields. */
    static Field declared(String name) {
        try {
            Field f = AbstractMap.class.getDeclaredField(name);
            f.setAccessible(true);
            return f;
        } catch (ReflectiveOperationException | RuntimeException e) {
            return null;
        }
    }

    /** `HashMap`'s own fields, for the contrast. */
    static Field declared2(String name) {
        try {
            Field f = HashMap.class.getDeclaredField(name);
            f.setAccessible(true);
            return f;
        } catch (ReflectiveOperationException | RuntimeException e) {
            return null;
        }
    }

    static void report(String tag, Map<?, ?> m, Field keySet, Field values, Field table, Field size) {
        System.out.println(tag + " " + describe(m, keySet, values, table, size));
    }

    static String describe(Map<?, ?> m, Field keySet, Field values, Field table, Field size) {
        return "size()=" + m.size()
                + " keySet=" + kind(get(keySet, m))
                + " values=" + kind(get(values, m))
                + " table=" + kind(get(table, m))
                + " sizeField=" + kind(get(size, m));
    }

    static String backingOf(Set<?> s, Field keySet, Field values, Field table, Field size) {
        try {
            Field mapField = HashSet.class.getDeclaredField("map");
            mapField.setAccessible(true);
            Object backing = mapField.get(s);
            if (!(backing instanceof Map)) {
                return "backing=" + kind(backing);
            }
            return describe((Map<?, ?>) backing, keySet, values, table, size);
        } catch (ReflectiveOperationException | RuntimeException e) {
            return "backing=<" + e.getClass().getSimpleName() + ">";
        }
    }

    static Object get(Field f, Object o) {
        if (f == null) {
            return "<no-field>";
        }
        try {
            return f.get(o);
        } catch (ReflectiveOperationException | RuntimeException e) {
            return "<" + e.getClass().getSimpleName() + ">";
        }
    }

    /**
     * The TYPE of what a field holds, never the contents: contents vary per
     * run (hashes, capacities), the type is the contract. An array reports its
     * component type and length bucket so a bucket table is unmistakable.
     */
    static String kind(Object v) {
        if (v == null) {
            return "null";
        }
        if (v instanceof String s && s.startsWith("<")) {
            return s;
        }
        Class<?> c = v.getClass();
        if (c.isArray()) {
            return "ARRAY[" + c.getComponentType().getSimpleName() + "]";
        }
        if (v instanceof Integer || v instanceof Long) {
            return "BOXED-" + c.getSimpleName();
        }
        return c.getName();
    }
}
