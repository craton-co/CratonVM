import java.io.*;
import java.util.*;
import java.util.concurrent.*;
import java.util.stream.*;

/**
 * Behavioural matrix for the JDK-only wave-2 lane L2 — "native_map_init's raw
 * MAP_FIELD_* branch -> by-name".
 *
 * The defect this covers is silent by construction: a raw slot index resolves,
 * type-checks, and lands on a DIFFERENT field, so nothing throws and nothing
 * logs. The overlay census sees the subset where the value KIND also
 * mismatches; it cannot see a same-kind wrong-slot write at all, and it
 * cannot see `Object(None)` written over a primitive. So the census is not
 * the verification — this is: every line below is a property of the real JDK
 * that a wrong-field write breaks, printed in a fixed order so the whole
 * transcript can be diffed byte-for-byte against HotSpot.
 *
 * Two things it deliberately does NOT do:
 *
 *  - it does not read `Hashtable.threshold` / `loadFactor` / `Properties.
 *    defaults` reflectively. Those are the fields the bug lands on, but JDK 25
 *    denies `setAccessible` on `java.base/java.util` without `--add-opens`,
 *    and a probe whose central assertion depends on a flag the VM under test
 *    may parse differently is a probe that reports on the flag;
 *  - it does not print sizes/hashes that legitimately differ between VMs.
 *
 * Instead each section drives real JDK bytecode that READS the fields in
 * question: `Properties.getProperty`/`propertyNames` walk the `defaults`
 * chain, `HashMap.writeObject` -> `internalWriteEntries` walks `table`, and
 * `keySet().spliterator()` reads `table` + `modCount` + `size`.
 *
 * Run it under HotSpot FIRST — the contract is whatever the real JDK does,
 * not what this file assumes.
 */
public class MapLayoutMatrixProbe {

    static int sections = 0, failed = 0;

    static void section(String name, Runnable body) {
        sections++;
        try {
            body.run();
        } catch (Throwable t) {
            failed++;
            System.out.println("SECTION-FAILED " + name + ": " + t);
        }
    }

    public static void main(String[] args) {
        section("props-defaults-chain", MapLayoutMatrixProbe::propsDefaultsChain);
        section("props-defaults-nested", MapLayoutMatrixProbe::propsDefaultsNested);
        section("props-defaults-mutation", MapLayoutMatrixProbe::propsDefaultsMutation);
        section("props-store-load", MapLayoutMatrixProbe::propsStoreLoad);
        section("props-errors", MapLayoutMatrixProbe::propsErrors);
        section("hashmap-serialize", MapLayoutMatrixProbe::hashmapSerialize);
        section("hashmap-copy-ctor", MapLayoutMatrixProbe::hashmapCopyCtor);
        section("hashmap-views", MapLayoutMatrixProbe::hashmapViews);
        section("hashmap-resize", MapLayoutMatrixProbe::hashmapResize);
        section("factory-copies", MapLayoutMatrixProbe::factoryCopies);
        section("hashtable-family", MapLayoutMatrixProbe::hashtableFamily);
        System.out.println("MAPLAYOUT sections=" + sections + " failed=" + failed);
    }

    // ---------------------------------------------------------------------
    // Properties.defaults — the L2 step-3 half.
    //
    // `native_props_init` wrote Object(None) to model slot 3, which is
    // `float loadFactor` on a real layout, and every reader read it back from
    // there: the walk got a Float, stopped, and the chain resolved nothing.
    // Nothing throws — `getProperty` just answers null, which is a legal
    // answer for an absent key.
    // ---------------------------------------------------------------------

    static void propsDefaultsChain() {
        Properties base = new Properties();
        base.setProperty("shared", "from-base");
        base.setProperty("only-base", "base");

        Properties child = new Properties(base);
        child.setProperty("shared", "from-child");
        child.setProperty("only-child", "child");

        // The chain is the whole point: `only-base` is NOT in `child`'s own
        // table, and can only be answered by following `defaults`.
        System.out.println("chain only-base=" + child.getProperty("only-base"));
        System.out.println("chain only-child=" + child.getProperty("only-child"));
        System.out.println("chain shared=" + child.getProperty("shared"));
        System.out.println("chain absent=" + child.getProperty("absent"));
        System.out.println("chain absent-with-default=" + child.getProperty("absent", "fallback"));

        // PAIRED property: the direct Map view must NOT see the defaults,
        // while the property accessors must. A fix that "works" by copying
        // the defaults into the child breaks this pair.
        System.out.println("chain size=" + child.size());
        System.out.println("chain get-via-map=" + child.get("only-base"));
        System.out.println("chain containsKey=" + child.containsKey("only-base"));

        System.out.println("chain stringPropertyNames=" + sorted(child.stringPropertyNames()));
        System.out.println("chain propertyNames=" + sortedEnum(child.propertyNames()));
    }

    static void propsDefaultsNested() {
        Properties a = new Properties();
        a.setProperty("level", "a");
        a.setProperty("a-only", "1");
        Properties b = new Properties(a);
        b.setProperty("level", "b");
        b.setProperty("b-only", "2");
        Properties c = new Properties(b);
        c.setProperty("level", "c");
        c.setProperty("c-only", "3");

        System.out.println("nested level=" + c.getProperty("level"));
        System.out.println("nested a-only=" + c.getProperty("a-only"));
        System.out.println("nested b-only=" + c.getProperty("b-only"));
        System.out.println("nested names=" + sorted(c.stringPropertyNames()));

        // An empty Properties with defaults: every answer comes from the chain.
        Properties empty = new Properties(c);
        System.out.println("nested empty-size=" + empty.size());
        System.out.println("nested empty-level=" + empty.getProperty("level"));
        System.out.println("nested empty-names=" + sorted(empty.stringPropertyNames()));
    }

    static void propsDefaultsMutation() {
        Properties base = new Properties();
        Properties child = new Properties(base);
        // The link is live, not a snapshot taken at construction.
        base.setProperty("late", "added-after-construction");
        System.out.println("mutation late=" + child.getProperty("late"));
        base.remove("late");
        System.out.println("mutation removed=" + child.getProperty("late"));

        // A non-String value in the defaults is invisible to getProperty
        // (the JDK checks `instanceof String`) but visible to the raw Map.
        base.put("boxed", Integer.valueOf(7));
        System.out.println("mutation boxed-getProperty=" + child.getProperty("boxed"));
        System.out.println("mutation boxed-map-get=" + base.get("boxed"));
        System.out.println("mutation boxed-in-names=" + child.stringPropertyNames().contains("boxed"));

        // `new Properties()` with no defaults must not invent a chain.
        Properties lone = new Properties();
        System.out.println("mutation lone=" + lone.getProperty("late"));
        System.out.println("mutation lone-names=" + sorted(lone.stringPropertyNames()));
    }

    static void propsStoreLoad() {
        Properties base = new Properties();
        base.setProperty("inherited", "yes");
        Properties child = new Properties(base);
        child.setProperty("own", "yes");
        StringWriter sw = new StringWriter();
        try {
            // store() writes only the receiver's OWN entries, never the
            // defaults — another paired property.
            child.store(sw, null);
            Properties back = new Properties();
            back.load(new StringReader(sw.toString()));
            System.out.println("storeload keys=" + sorted(back.stringPropertyNames()));
            System.out.println("storeload own=" + back.getProperty("own"));
            System.out.println("storeload inherited=" + back.getProperty("inherited"));
        } catch (IOException e) {
            throw new RuntimeException(e);
        }
    }

    static void propsErrors() {
        Properties p = new Properties();
        p.setProperty("k", "v");
        System.out.println("errors getProperty-null=" + thrown(() -> p.getProperty(null)));
        System.out.println("errors setProperty-null-value=" + thrown(() -> p.setProperty("k", null)));
        System.out.println("errors put-null-key=" + thrown(() -> p.put(null, "v")));
        System.out.println("errors load-null=" + thrown(() -> {
            try {
                p.load((InputStream) null);
            } catch (IOException e) {
                throw new RuntimeException(e);
            }
        }));
        // `new Properties(null)` is legal and means "no defaults".
        System.out.println("errors null-defaults=" + thrown(() -> {
            Properties q = new Properties(null);
            if (q.getProperty("x") != null) throw new IllegalStateException("unexpected");
        }));
    }

    // ---------------------------------------------------------------------
    // HashMap.table — the L2 step-2 half.
    //
    // `HashMap.writeObject` is NOT on the force-native override list, so it
    // runs as real bytecode: `internalWriteEntries` walks `this.table`
    // directly. An Int in that field is an `arraylength` on an int; a stale
    // or absent array serializes an empty map with a non-zero size header.
    // ---------------------------------------------------------------------

    static void hashmapSerialize() {
        for (int n : new int[] {0, 1, 12, 13, 40}) {
            Map<String, Integer> m = new HashMap<>();
            for (int i = 0; i < n; i++) m.put("k" + i, i);
            System.out.println("serialize n=" + n + " " + roundTrip(m));
        }
        // The copy constructor's own product must survive the same trip.
        Map<String, Integer> src = new HashMap<>();
        for (int i = 0; i < 20; i++) src.put("s" + i, i);
        System.out.println("serialize copy " + roundTrip(new HashMap<>(src)));
        System.out.println("serialize linked " + roundTrip(new LinkedHashMap<>(src)));
        System.out.println("serialize hashtable " + roundTrip(new Hashtable<>(src)));
        System.out.println("serialize props " + roundTrip(withProps()));

        Map<String, Object> bad = new HashMap<>();
        bad.put("k", new Object());
        System.out.println("serialize non-serializable=" + thrown(() -> writeOnly(bad)));
    }

    static Properties withProps() {
        Properties p = new Properties();
        p.setProperty("a", "1");
        p.setProperty("b", "2");
        return p;
    }

    static void hashmapCopyCtor() {
        Map<String, Integer> src = new LinkedHashMap<>();
        for (int i = 0; i < 30; i++) src.put("k" + i, i);
        Map<String, Integer> copy = new HashMap<>(src);
        System.out.println("copy size=" + copy.size() + " equals=" + copy.equals(src));
        System.out.println("copy keys-sorted=" + sorted(copy.keySet()));
        System.out.println("copy sum=" + copy.values().stream().mapToInt(Integer::intValue).sum());
        // A copy of an empty map, then grown past its threshold: this is the
        // path where the constructor's own capacity write and the first
        // resize have to agree about where the table lives.
        Map<String, Integer> grown = new HashMap<>(new HashMap<String, Integer>());
        for (int i = 0; i < 100; i++) grown.put("g" + i, i);
        System.out.println("copy grown=" + grown.size() + " probe=" + grown.get("g99"));
        System.out.println("copy of-props=" + new HashMap<>(withProps()).size());
        System.out.println("copy of-hashtable=" + new HashMap<>(new Hashtable<>(src)).size());
    }

    static void hashmapViews() {
        Map<String, Integer> m = new HashMap<>();
        for (int i = 0; i < 50; i++) m.put("v" + i, i);
        // Spliterator paths read `table`, `size` and `modCount` as bytecode.
        System.out.println("views parallel-sum="
                + m.entrySet().parallelStream().mapToInt(Map.Entry::getValue).sum());
        System.out.println("views key-chars="
                + m.keySet().stream().mapToInt(String::length).sum());
        System.out.println("views spliterator-estimate="
                + m.keySet().spliterator().estimateSize());
        System.out.println("views sorted-keys=" + sorted(m.keySet()).size());
        // CME is a modCount property — it must still fire.
        System.out.println("views cme=" + thrown(() -> {
            for (String k : m.keySet()) m.put(k + "!", 0);
        }));
        Set<String> keys = m.keySet();
        keys.remove("v0");
        System.out.println("views write-through=" + m.containsKey("v0") + " size=" + m.size());
    }

    static void hashmapResize() {
        // Cross every resize boundary with a deterministic key set, then read
        // everything back. A table published to the wrong slot loses entries
        // silently; nothing throws.
        Map<Integer, Integer> m = new HashMap<>();
        int lost = 0;
        for (int i = 0; i < 5000; i++) {
            m.put(i, i * 2);
        }
        for (int i = 0; i < 5000; i++) {
            Integer v = m.get(i);
            if (v == null || v != i * 2) lost++;
        }
        System.out.println("resize size=" + m.size() + " lost=" + lost);
        int removed = 0;
        for (int i = 0; i < 5000; i += 3) if (m.remove(i) != null) removed++;
        System.out.println("resize removed=" + removed + " remaining=" + m.size());
        // Explicit capacities, including the ones that make `table` and the
        // legacy `capacity` slot collide.
        for (int cap : new int[] {0, 1, 16, 17, 64}) {
            Map<String, String> c = new HashMap<>(cap);
            for (int i = 0; i < 40; i++) c.put("c" + i, "" + i);
            System.out.println("resize cap=" + cap + " size=" + c.size() + " probe=" + c.get("c39"));
        }
    }

    static void factoryCopies() {
        Map<String, Integer> of = Map.of("a", 1, "b", 2, "c", 3);
        System.out.println("factory of-size=" + of.size() + " keys=" + sorted(of.keySet()));
        System.out.println("factory copyOf=" + sorted(Map.copyOf(of).keySet()));
        System.out.println("factory into-hashmap=" + sorted(new HashMap<>(of).keySet()));
        Set<String> set = Set.of("x", "y", "z");
        System.out.println("factory set=" + sorted(set) + " into=" + sorted(new HashSet<>(set)));
        System.out.println("factory collector="
                + sorted(Stream.of("p", "q", "r").collect(Collectors.toSet())));
        System.out.println("factory groupby="
                + new TreeMap<>(Stream.of("aa", "ab", "bc")
                        .collect(Collectors.groupingBy(s -> s.substring(0, 1)))));
        Set<String> keySet = ConcurrentHashMap.newKeySet();
        keySet.add("k1");
        keySet.add("k2");
        System.out.println("factory chm-keyset=" + sorted(keySet));
        System.out.println("factory unmodifiable="
                + thrown(() -> Collections.unmodifiableMap(new HashMap<>(of)).put("d", 4)));
    }

    static void hashtableFamily() {
        // Hashtable takes the OTHER branch (CF_HASHTABLE_LAYOUT), which this
        // change deliberately leaves alone. It is here as the control: if
        // these lines move, the change reached further than intended.
        Hashtable<String, Integer> h = new Hashtable<>();
        for (int i = 0; i < 40; i++) h.put("h" + i, i);
        System.out.println("hashtable size=" + h.size() + " probe=" + h.get("h39"));
        System.out.println("hashtable keys=" + sortedEnum(h.keys()).size());
        System.out.println("hashtable null-key=" + thrown(() -> h.put(null, 1)));
        System.out.println("hashtable null-value=" + thrown(() -> h.put("k", null)));
        Hashtable<String, Integer> sized = new Hashtable<>(3);
        for (int i = 0; i < 20; i++) sized.put("s" + i, i);
        System.out.println("hashtable sized=" + sized.size() + " probe=" + sized.get("s19"));
        System.out.println("hashtable equals=" + new Hashtable<>(h).equals(h));
        Properties p = withProps();
        System.out.println("hashtable props-as-map=" + sorted(p.keySet()));
        System.out.println("hashtable props-size=" + p.size());
    }

    // ---------------------------------------------------------------------
    // Helpers. Every one of them is order-normalising: bucket order is a
    // legitimate difference between implementations of `table`, and a probe
    // that prints it diffs forever for the wrong reason.
    // ---------------------------------------------------------------------

    static List<String> sorted(Collection<?> c) {
        List<String> out = new ArrayList<>();
        for (Object o : c) out.add(String.valueOf(o));
        Collections.sort(out);
        return out;
    }

    static List<String> sortedEnum(Enumeration<?> e) {
        List<String> out = new ArrayList<>();
        while (e.hasMoreElements()) out.add(String.valueOf(e.nextElement()));
        Collections.sort(out);
        return out;
    }

    /** Name of the throwable a body raises, or "none". */
    static String thrown(Runnable body) {
        try {
            body.run();
            return "none";
        } catch (Throwable t) {
            Throwable root = t;
            while (root instanceof RuntimeException && root.getCause() != null
                    && root.getClass() == RuntimeException.class) {
                root = root.getCause();
            }
            return root.getClass().getName();
        }
    }

    static void writeOnly(Object o) {
        try (ObjectOutputStream oos = new ObjectOutputStream(new ByteArrayOutputStream())) {
            oos.writeObject(o);
        } catch (IOException e) {
            throw new RuntimeException(e);
        }
    }

    /**
     * Serialize and read back, reporting size, equality and a sorted key
     * digest. `HashMap.writeObject` walks `table` as real bytecode, so a
     * wrong `table` shows up here as a size/equality divergence rather than
     * as an exception.
     */
    static String roundTrip(Map<?, ?> m) {
        try {
            ByteArrayOutputStream bos = new ByteArrayOutputStream();
            try (ObjectOutputStream oos = new ObjectOutputStream(bos)) {
                oos.writeObject(m);
            }
            Object back;
            try (ObjectInputStream ois =
                    new ObjectInputStream(new ByteArrayInputStream(bos.toByteArray()))) {
                back = ois.readObject();
            }
            Map<?, ?> r = (Map<?, ?>) back;
            return "class=" + r.getClass().getName()
                    + " size=" + r.size()
                    + " equal=" + r.equals(m)
                    + " keys=" + sorted(r.keySet()).size();
        } catch (IOException | ClassNotFoundException e) {
            return "threw=" + e.getClass().getName();
        }
    }
}
