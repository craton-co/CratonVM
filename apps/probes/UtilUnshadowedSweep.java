import java.util.*;
import java.util.stream.*;

/** L3 — the `java.util` classes this VM does NOT shadow.
 *
 *  The registry carries no native for `BitSet`, `StringJoiner`, `EnumSet`,
 *  `WeakHashMap`, `IdentityHashMap`, `StringTokenizer`, `Objects` or
 *  `Comparator`, so every row here runs java.base's own bytecode on both arms.
 *  A divergence therefore cannot be a shadow defect -- it is the INTERPRETER,
 *  the JIT or the collector getting something wrong under real library code,
 *  which is the harder kind to find and the kind no shadow census can point at.
 *
 *  That also makes a clean run worth having: it is a lock on the VM under a few
 *  thousand lines of real `java.util`, taken through data structures with bit
 *  twiddling, identity semantics, weak references and enum ordinals.
 *
 *  DETERMINISM. `IdentityHashMap` and `WeakHashMap` do not specify iteration
 *  order, so every row over them sorts its result. `WeakHashMap`'s keys are held
 *  in a strong array for the lifetime of the probe, so nothing can be collected
 *  mid-row; the one row that WANTS a collection says so and asks only for a
 *  bound. Nothing prints a hash code that is not content-derived.
 */
public class UtilUnshadowedSweep {
    static int rows = 0;

    interface Val { Object call() throws Throwable; }

    static String esc(String s) {
        return s == null ? "null" : s.replace("\n", "\\n").replace("\r", "\\r");
    }

    static void p(String tag, Object v) {
        rows++;
        System.out.println(rows + " " + esc(tag) + " |" + esc(String.valueOf(v)) + "|");
    }

    static void tv(String tag, Val c) {
        try { p(tag, c.call()); }
        catch (Throwable e) {
            p(tag, "THREW " + e.getClass().getName() + " msg=" + esc(e.getMessage()));
        }
    }

    static String sorted(Collection<?> c) {
        List<String> l = new ArrayList<>();
        for (Object o : c) l.add(String.valueOf(o));
        Collections.sort(l);
        return l.toString();
    }

    enum Colour { RED, GREEN, BLUE, AMBER }

    public static void main(String[] args) {
        // ---- BitSet: the bit twiddling the interpreter and the JIT both touch
        tv("bs set/get", () -> { BitSet b = new BitSet(); b.set(3); return b.get(3) + "/" + b.get(4); });
        tv("bs toString", () -> { BitSet b = new BitSet(); b.set(1); b.set(5); b.set(64); return b.toString(); });
        tv("bs cardinality", () -> { BitSet b = new BitSet(); b.set(0, 10); return b.cardinality(); });
        tv("bs length vs size", () -> { BitSet b = new BitSet(); b.set(70); return b.length() + "/" + b.size(); });
        tv("bs clear range", () -> { BitSet b = new BitSet(); b.set(0, 10); b.clear(2, 5); return b.toString(); });
        tv("bs flip", () -> { BitSet b = new BitSet(); b.set(0, 4); b.flip(1, 3); return b.toString(); });
        tv("bs and", () -> { BitSet a = bits(1, 2, 3); BitSet c = bits(2, 3, 4); a.and(c); return a.toString(); });
        tv("bs or", () -> { BitSet a = bits(1, 2); BitSet c = bits(3); a.or(c); return a.toString(); });
        tv("bs xor", () -> { BitSet a = bits(1, 2); BitSet c = bits(2, 3); a.xor(c); return a.toString(); });
        tv("bs andNot", () -> { BitSet a = bits(1, 2, 3); BitSet c = bits(2); a.andNot(c); return a.toString(); });
        tv("bs intersects", () -> bits(1, 2).intersects(bits(2, 9)));
        tv("bs nextSetBit", () -> { BitSet b = bits(5, 40, 200); return b.nextSetBit(0) + "," + b.nextSetBit(6) + "," + b.nextSetBit(201); });
        tv("bs nextClearBit", () -> { BitSet b = bits(0, 1, 2); return b.nextClearBit(0); });
        tv("bs previousSetBit", () -> bits(3, 77).previousSetBit(100));
        tv("bs get range", () -> bits(1, 2, 3, 9).get(1, 4).toString());
        tv("bs stream", () -> bits(2, 4, 8).stream().boxed().toList().toString());
        tv("bs toLongArray", () -> Arrays.toString(bits(0, 64).toLongArray()));
        tv("bs toByteArray", () -> Arrays.toString(bits(0, 9).toByteArray()));
        tv("bs valueOf longs", () -> BitSet.valueOf(new long[] {5L}).toString());
        tv("bs equals/hash", () -> bits(1, 2).equals(bits(1, 2)) + "/"
                + (bits(1, 2).hashCode() == bits(1, 2).hashCode()));
        tv("bs isEmpty", () -> new BitSet().isEmpty() + "/" + bits(1).isEmpty());
        tv("bs negative index", () -> { BitSet b = new BitSet(); b.set(-1); return "no-throw"; });

        // ---- StringJoiner
        tv("sj basic", () -> new StringJoiner(",").add("a").add("b").toString());
        tv("sj prefix/suffix", () -> new StringJoiner(",", "[", "]").add("a").toString());
        tv("sj empty", () -> new StringJoiner(",", "[", "]").toString());
        tv("sj emptyValue", () -> new StringJoiner(",", "[", "]").setEmptyValue("NONE").toString());
        tv("sj length", () -> new StringJoiner(",", "<", ">").add("ab").length());
        tv("sj merge", () -> {
            StringJoiner a = new StringJoiner(",", "[", "]").add("1");
            StringJoiner b = new StringJoiner("-", "{", "}").add("x").add("y");
            return a.merge(b).toString();
        });
        tv("sj merge empty", () -> new StringJoiner(",").add("a")
                .merge(new StringJoiner("-")).toString());
        tv("sj add null", () -> new StringJoiner(",").add(null).toString());

        // ---- EnumSet / EnumMap: ordinal-ordered, so printable directly
        tv("es allOf", () -> EnumSet.allOf(Colour.class).toString());
        tv("es noneOf", () -> EnumSet.noneOf(Colour.class).toString());
        tv("es of", () -> EnumSet.of(Colour.BLUE, Colour.RED).toString());
        tv("es range", () -> EnumSet.range(Colour.GREEN, Colour.AMBER).toString());
        tv("es complementOf", () -> EnumSet.complementOf(EnumSet.of(Colour.RED)).toString());
        tv("es copyOf collection", () -> EnumSet.copyOf(List.of(Colour.AMBER, Colour.RED)).toString());
        tv("es contains/remove", () -> {
            EnumSet<Colour> s = EnumSet.of(Colour.RED, Colour.BLUE);
            boolean had = s.remove(Colour.RED);
            return had + " " + s;
        });
        tv("em put/get/order", () -> {
            EnumMap<Colour, Integer> m = new EnumMap<>(Colour.class);
            m.put(Colour.BLUE, 3); m.put(Colour.RED, 1);
            return m.toString() + " keys=" + m.keySet();
        });
        tv("em null key", () -> new EnumMap<Colour, Integer>(Colour.class).put(null, 1));

        // ---- IdentityHashMap: equal but distinct keys stay distinct
        tv("ihm distinct equal keys", () -> {
            String a = new String("k"), b = new String("k");
            IdentityHashMap<String, Integer> m = new IdentityHashMap<>();
            m.put(a, 1); m.put(b, 2);
            return m.size() + " " + m.get(a) + " " + m.get(b);
        });
        tv("ihm same ref overwrites", () -> {
            String a = "k";
            IdentityHashMap<String, Integer> m = new IdentityHashMap<>();
            m.put(a, 1); m.put(a, 2);
            return m.size() + " " + m.get(a);
        });
        tv("ihm keySet sorted", () -> {
            IdentityHashMap<String, Integer> m = new IdentityHashMap<>();
            m.put("x", 1); m.put("y", 2);
            return sorted(m.keySet());
        });
        tv("ihm null key", () -> {
            IdentityHashMap<String, Integer> m = new IdentityHashMap<>();
            m.put(null, 7);
            return m.size() + " " + m.get(null);
        });

        // ---- WeakHashMap, with the keys held STRONGLY so nothing is collected
        tv("whm basic", () -> {
            String[] keep = {new String("a"), new String("b")};
            WeakHashMap<String, Integer> m = new WeakHashMap<>();
            m.put(keep[0], 1); m.put(keep[1], 2);
            return m.size() + " " + sorted(m.keySet()) + " " + m.get(keep[0]);
        });
        tv("whm remove", () -> {
            String k = new String("a");
            WeakHashMap<String, Integer> m = new WeakHashMap<>();
            m.put(k, 1);
            return m.remove(k) + " size=" + m.size();
        });
        tv("whm equal keys collapse", () -> {
            String a = new String("k"), b = new String("k");
            WeakHashMap<String, Integer> m = new WeakHashMap<>();
            m.put(a, 1); m.put(b, 2);
            return m.size() + " " + m.get(a);
        });

        // ---- StringTokenizer
        tv("st basic", () -> {
            StringTokenizer t = new StringTokenizer("a b  c");
            StringBuilder sb = new StringBuilder();
            while (t.hasMoreTokens()) sb.append('[').append(t.nextToken()).append(']');
            return sb.toString();
        });
        tv("st countTokens", () -> new StringTokenizer("a,b,c", ",").countTokens());
        tv("st returnDelims", () -> {
            StringTokenizer t = new StringTokenizer("a,b", ",", true);
            StringBuilder sb = new StringBuilder();
            while (t.hasMoreTokens()) sb.append('[').append(t.nextToken()).append(']');
            return sb.toString();
        });
        tv("st past end", () -> new StringTokenizer("").nextToken());
        tv("st empty delims", () -> new StringTokenizer("abc", "").countTokens());

        // ---- Objects
        tv("Objects.equals nulls", () -> Objects.equals(null, null) + "/" + Objects.equals(null, "a"));
        tv("Objects.deepEquals arrays", () -> Objects.deepEquals(new int[] {1, 2}, new int[] {1, 2}));
        tv("Objects.hash", () -> Objects.hash("a", 1, null));
        tv("Objects.hashCode null", () -> Objects.hashCode(null));
        tv("Objects.toString default", () -> Objects.toString(null, "D"));
        tv("Objects.requireNonNull msg", () -> Objects.requireNonNull(null, "boom"));
        tv("Objects.requireNonNull plain", () -> Objects.requireNonNull(null));
        tv("Objects.requireNonNullElse", () -> Objects.requireNonNullElse(null, "fb"));
        tv("Objects.compare", () -> Objects.compare("a", "b", Comparator.naturalOrder()));
        tv("Objects.checkIndex ok", () -> Objects.checkIndex(1, 3));
        tv("Objects.checkIndex bad", () -> Objects.checkIndex(5, 3));
        tv("Objects.isNull/nonNull", () -> Objects.isNull(null) + "/" + Objects.nonNull(null));
        tv("Objects.requireNonNullElseGet", () -> Objects.requireNonNullElseGet(null, () -> "s"));
        tv("Objects.toString(obj)", () -> Objects.toString(42));

        // ---- Comparator combinators
        tv("cmp naturalOrder", () -> Comparator.<String>naturalOrder().compare("a", "b"));
        tv("cmp reverseOrder", () -> Comparator.<String>reverseOrder().compare("a", "b"));
        tv("cmp comparing", () -> {
            List<String> l = new ArrayList<>(List.of("bbb", "a", "cc"));
            l.sort(Comparator.comparing(String::length));
            return l.toString();
        });
        tv("cmp thenComparing", () -> {
            List<String> l = new ArrayList<>(List.of("bb", "aa", "c"));
            l.sort(Comparator.comparingInt(String::length).thenComparing(Comparator.naturalOrder()));
            return l.toString();
        });
        tv("cmp reversed", () -> {
            List<String> l = new ArrayList<>(List.of("a", "c", "b"));
            l.sort(Comparator.<String>naturalOrder().reversed());
            return l.toString();
        });
        tv("cmp nullsFirst", () -> {
            List<String> l = new ArrayList<>(Arrays.asList("b", null, "a"));
            l.sort(Comparator.nullsFirst(Comparator.naturalOrder()));
            return l.toString();
        });
        tv("cmp nullsLast", () -> {
            List<String> l = new ArrayList<>(Arrays.asList("b", null, "a"));
            l.sort(Comparator.nullsLast(Comparator.naturalOrder()));
            return l.toString();
        });
        tv("cmp comparingDouble", () -> Comparator.<String>comparingDouble(String::length)
                .compare("aa", "b"));

        // ---- the JDK 21 sequenced surface, on classes with no shadow of it
        tv("seq List reversed", () -> new ArrayList<>(List.of(1, 2, 3)).reversed().toString());
        tv("seq List getFirst/Last", () -> {
            List<Integer> l = new ArrayList<>(List.of(1, 2, 3));
            return l.getFirst() + "/" + l.getLast();
        });
        tv("seq LinkedHashSet reversed", () ->
                new LinkedHashSet<>(List.of(1, 2, 3)).reversed().toString());
        tv("seq empty getFirst", () -> new ArrayList<Integer>().getFirst());

        System.out.println("DONE UtilUnshadowedSweep");
    }

    static BitSet bits(int... ix) {
        BitSet b = new BitSet();
        for (int i : ix) b.set(i);
        return b;
    }
}
