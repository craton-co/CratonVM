import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.ObjectInputStream;
import java.io.ObjectOutputStream;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.HashSet;
import java.util.Iterator;
import java.util.LinkedHashMap;
import java.util.LinkedHashSet;
import java.util.List;
import java.util.Map;
import java.util.Set;

/**
 * End-to-end witness that CratonVM's {@code LinkedHashMap} nodes ARE the real
 * JDK node class.
 *
 * <p>{@code lhm_alloc_node} used to mint {@code java.util.LinkedHashMap$Node},
 * a class the real JDK does not have — its nested node type is
 * {@code java.util.LinkedHashMap$Entry}. The field layout was already the real
 * one ({@code hash, key, value, next, before, after}); only the class identity
 * was invented, which is what turned the Kafka embedded-KRaft defect into
 *
 * <pre>
 * ClassCastException: java.util.LinkedHashMap$Node cannot be cast to
 *                     java.util.LinkedHashMap$Entry
 * </pre>
 *
 * <p>and what forced the eldest-entry hook to hand {@code removeEldestEntry} a
 * copied {@code AbstractMap$SimpleImmutableEntry}: the invented class declares
 * no methods, so {@code eldest.getKey()} was a {@code NoSuchMethodError}.
 *
 * <p>Every assertion below is a plain JDK-contract assertion, so the same run is
 * meaningful on HotSpot (the control) and on CratonVM.
 *
 * <pre>
 * javac -d /tmp/probe probes/LinkedHashMapNodeProbe.java
 * java  -cp /tmp/probe LinkedHashMapNodeProbe                 # control
 * cratonvm --java-home &lt;jdk&gt; -cp /tmp/probe LinkedHashMapNodeProbe
 * </pre>
 */
public class LinkedHashMapNodeProbe {

    private static int failures = 0;

    private static void check(String what, Object expected, Object actual) {
        boolean ok = expected == null ? actual == null : expected.equals(actual);
        System.out.println((ok ? "  ok   " : "  FAIL ") + what
                + " expected=" + expected + " actual=" + actual);
        if (!ok) {
            failures++;
        }
    }

    private static void checkTrue(String what, boolean actual) {
        check(what, Boolean.TRUE, Boolean.valueOf(actual));
    }

    // ------------------------------------------------------------------
    // 1. removeEldestEntry sees the real node
    // ------------------------------------------------------------------

    /** Records everything the hook can observe about the entry it is handed. */
    static final class Observer extends LinkedHashMap<String, String> {
        private static final long serialVersionUID = 1L;

        String className;
        Object key;
        Object value;
        String asString;
        int hash;
        boolean isMapEntry;
        int calls;
        /** When >= 0, evict once the map grows past this many entries. */
        int maxEntries = -1;
        /** When non-null, the hook calls setValue with it on the eldest. */
        String setValueTo;

        @Override
        protected boolean removeEldestEntry(Map.Entry<String, String> eldest) {
            calls++;
            className = eldest.getClass().getName();
            key = eldest.getKey();
            value = eldest.getValue();
            asString = eldest.toString();
            hash = eldest.hashCode();
            isMapEntry = eldest instanceof Map.Entry;
            if (setValueTo != null) {
                String target = setValueTo;
                setValueTo = null; // one shot; setValue must not re-enter
                eldest.setValue(target);
            }
            return maxEntries >= 0 && size() > maxEntries;
        }
    }

    private static void eldestIsTheRealNode() {
        System.out.println("== removeEldestEntry receives the real node");
        Observer m = new Observer();
        m.put("a", "1");
        m.put("b", "2");

        check("hook was called", Integer.valueOf(2), Integer.valueOf(m.calls));
        // The class the JDK actually uses. This is the whole point of the probe.
        check("eldest.getClass()", "java.util.LinkedHashMap$Entry", m.className);
        checkTrue("eldest instanceof Map.Entry", m.isMapEntry);
        // Head is the FIRST-inserted entry (insertion order).
        check("eldest.getKey()", "a", m.key);
        check("eldest.getValue()", "1", m.value);
        check("eldest.toString()", "a=1", m.asString);
        check("eldest.hashCode()", Integer.valueOf("a".hashCode() ^ "1".hashCode()),
                Integer.valueOf(m.hash));
    }

    private static void eldestSetValueWritesThroughToTheMap() {
        System.out.println("== eldest.setValue mutates the map (not a detached copy)");
        Observer m = new Observer();
        m.put("a", "1");
        m.setValueTo = "REWRITTEN";
        m.put("b", "2"); // fires the hook, which rewrites the eldest's value

        check("map.get(a)", "REWRITTEN", m.get("a"));
        check("map.get(b)", "2", m.get("b"));
        check("size", Integer.valueOf(2), Integer.valueOf(m.size()));
        check("iteration order", "[a=REWRITTEN, b=2]", entryListOf(m).toString());
    }

    private static void eldestEvictionStillWorks() {
        System.out.println("== removeEldestEntry -> true evicts the eldest (LRU cache)");
        Observer m = new Observer();
        m.maxEntries = 3;
        for (int i = 1; i <= 6; i++) {
            m.put("k" + i, "v" + i);
        }
        check("size capped", Integer.valueOf(3), Integer.valueOf(m.size()));
        check("oldest evicted", null, m.get("k1"));
        check("oldest evicted", null, m.get("k3"));
        check("newest kept", "v6", m.get("k6"));
        check("iteration order", "[k4=v4, k5=v5, k6=v6]", entryListOf(m).toString());
    }

    /**
     * Hibernate's {@code BoundedConcurrentHashMap.LRU} shape: the override
     * removes the eldest ITSELF (its eviction listener calls back into
     * {@code this.remove}) and then reports true, so the caller's own removal
     * has to be an idempotent no-op. This is the path the eldest-key pin in
     * {@code native_lhm_put_evict} exists for — the key is read before the
     * dispatch precisely because the node may be unlinked by the time it
     * returns.
     */
    static final class ReentrantEvictor extends LinkedHashMap<String, String> {
        private static final long serialVersionUID = 1L;
        boolean andReportTrue;
        int evicted;

        @Override
        protected boolean removeEldestEntry(Map.Entry<String, String> eldest) {
            if (size() <= 3) {
                return false;
            }
            remove(eldest.getKey()); // reentrant removal, exactly like Hibernate's LRU
            evicted++;
            return andReportTrue;
        }
    }

    private static void reentrantEvictionIsIdempotent() {
        System.out.println("== override removes the eldest reentrantly");
        for (int variant = 0; variant < 2; variant++) {
            boolean reportTrue = variant == 0;
            ReentrantEvictor m = new ReentrantEvictor();
            m.andReportTrue = reportTrue;
            for (int i = 1; i <= 6; i++) {
                m.put("k" + i, "v" + i);
            }
            String tag = " (reports " + reportTrue + ")";
            check("size" + tag, Integer.valueOf(3), Integer.valueOf(m.size()));
            check("evictions" + tag, Integer.valueOf(3), Integer.valueOf(m.evicted));
            check("oldest gone" + tag, null, m.get("k1"));
            check("newest kept" + tag, "v6", m.get("k6"));
            check("order" + tag, "[k4=v4, k5=v5, k6=v6]", entryListOf(m).toString());
        }
    }

    // ------------------------------------------------------------------
    // 2. The node still behaves as a LinkedHashMap node everywhere else
    // ------------------------------------------------------------------

    private static List<String> entryListOf(Map<String, String> m) {
        List<String> out = new ArrayList<>();
        for (Map.Entry<String, String> e : m.entrySet()) {
            out.add(e.getKey() + "=" + e.getValue());
        }
        return out;
    }

    private static void insertionOrderAndBasicOps() {
        System.out.println("== insertion order and basic operations");
        LinkedHashMap<String, String> m = new LinkedHashMap<>();
        m.put("a", "1");
        m.put("b", "2");
        m.put("c", "3");
        check("order", "[a=1, b=2, c=3]", entryListOf(m).toString());

        // Re-putting an existing key keeps its original position.
        m.put("a", "9");
        check("order after re-put", "[a=9, b=2, c=3]", entryListOf(m).toString());

        // The conditional mutators — the defect-1 path through
        // HashMap.removeNode -> LinkedHashMap.afterNodeRemoval, whose first
        // statement is `(LinkedHashMap.Entry<K,V>) e`.
        check("remove(b,wrong)", Boolean.FALSE, Boolean.valueOf(m.remove("b", "xx")));
        check("remove(b,2)", Boolean.TRUE, Boolean.valueOf(m.remove("b", "2")));
        check("order after remove", "[a=9, c=3]", entryListOf(m).toString());
        check("replace(c,30)", "3", m.replace("c", "30"));
        check("replace(c,30,300)", Boolean.TRUE, Boolean.valueOf(m.replace("c", "30", "300")));
        check("order after replace", "[a=9, c=300]", entryListOf(m).toString());

        // Removing the head and the tail exercises both ends of the
        // before/after chain the real Entry class declares.
        m.put("d", "4");
        m.remove("a");
        check("order after head removal", "[c=300, d=4]", entryListOf(m).toString());
        m.remove("d");
        check("order after tail removal", "[c=300]", entryListOf(m).toString());
    }

    private static void entrySetSetValueWritesThrough() {
        System.out.println("== entrySet().iterator() setValue writes through");
        LinkedHashMap<String, String> m = new LinkedHashMap<>();
        m.put("a", "1");
        m.put("b", "2");
        Iterator<Map.Entry<String, String>> it = m.entrySet().iterator();
        Map.Entry<String, String> first = it.next();
        check("first key", "a", first.getKey());
        check("setValue returns old", "1", first.setValue("11"));
        check("map sees it", "11", m.get("a"));
        check("order", "[a=11, b=2]", entryListOf(m).toString());
    }

    private static void accessOrderIsLru() {
        System.out.println("== access-order LinkedHashMap");
        LinkedHashMap<String, String> m = new LinkedHashMap<>(16, 0.75f, true);
        m.put("a", "1");
        m.put("b", "2");
        m.put("c", "3");
        m.get("a");
        check("after get(a)", "[b=2, c=3, a=1]", entryListOf(m).toString());
        m.put("b", "22");
        check("after put(b)", "[c=3, a=1, b=22]", entryListOf(m).toString());
    }

    private static void serializationRoundTrip() {
        System.out.println("== serialization round trip");
        LinkedHashMap<String, String> m = new LinkedHashMap<>();
        m.put("a", "1");
        m.put("b", "2");
        m.put("c", "3");
        try {
            ByteArrayOutputStream bos = new ByteArrayOutputStream();
            ObjectOutputStream oos = new ObjectOutputStream(bos);
            oos.writeObject(m);
            oos.close();
            ObjectInputStream ois =
                    new ObjectInputStream(new ByteArrayInputStream(bos.toByteArray()));
            @SuppressWarnings("unchecked")
            LinkedHashMap<String, String> back = (LinkedHashMap<String, String>) ois.readObject();
            ois.close();
            check("round-tripped size", Integer.valueOf(3), Integer.valueOf(back.size()));
            check("round-tripped order", "[a=1, b=2, c=3]", entryListOf(back).toString());
            check("round-tripped equals", Boolean.TRUE, Boolean.valueOf(m.equals(back)));
        } catch (Exception e) {
            System.out.println("  FAIL serialization threw " + e);
            e.printStackTrace(System.out);
            failures++;
        }
    }

    /**
     * A {@code Set} is a map whose values are a PRESENT marker, and CratonVM's
     * set layer writes a raw {@code Int(1)} there. Once the node binds to the
     * real JDK class, that slot is {@code V value -> Ljava/lang/Object;}, so
     * the descriptor-aware write path coerces the marker to null — and
     * {@code add}/{@code remove} both decide membership from "was the previous
     * value null". Symptom: {@code remove(x)} deletes the element and returns
     * {@code false}, which is what {@code Resource.Builder.onBuildMethod}'s
     * {@code checkState(methodBuilders.remove(builder))} caught in Jersey.
     *
     * <p>{@code CopyOnWriteArraySet} shares the same LinkedHashMap backing;
     * {@code HashSet} is here as the control that never broke.
     */
    private static void setMembershipIsReportedCorrectly() {
        System.out.println("== Set add/remove report membership (PRESENT marker)");
        List<Set<String>> sets = new ArrayList<>();
        sets.add(new LinkedHashSet<>());
        sets.add(new HashSet<>());
        sets.add(new java.util.concurrent.CopyOnWriteArraySet<>());
        for (Set<String> s : sets) {
            String n = "  " + s.getClass().getSimpleName();
            check(n + " add(new)", Boolean.TRUE, Boolean.valueOf(s.add("a")));
            check(n + " add(dup)", Boolean.FALSE, Boolean.valueOf(s.add("a")));
            check(n + " size", Integer.valueOf(1), Integer.valueOf(s.size()));
            check(n + " contains", Boolean.TRUE, Boolean.valueOf(s.contains("a")));
            check(n + " remove(present)", Boolean.TRUE, Boolean.valueOf(s.remove("a")));
            check(n + " remove(again)", Boolean.FALSE, Boolean.valueOf(s.remove("a")));
            check(n + " empty", Boolean.TRUE, Boolean.valueOf(s.isEmpty()));
            // Identity-keyed elements, the Jersey shape: many adds, then remove
            // each one and require every call to report true.
            int reported = 0;
            List<Object> objs = new ArrayList<>();
            Set<Object> t = s.getClass() == HashSet.class
                    ? new HashSet<>()
                    : (s.getClass() == LinkedHashSet.class
                            ? new LinkedHashSet<>()
                            : new java.util.concurrent.CopyOnWriteArraySet<>());
            for (int i = 0; i < 40; i++) {
                Object o = new Object();
                objs.add(o);
                t.add(o);
            }
            for (Object o : objs) {
                if (t.remove(o)) {
                    reported++;
                }
            }
            check(n + " 40 identity removes report true", Integer.valueOf(40),
                    Integer.valueOf(reported));
            check(n + " emptied", Boolean.TRUE, Boolean.valueOf(t.isEmpty()));
        }
        // The map value itself must survive the round trip unchanged.
        Map<String, Object> m = new LinkedHashMap<>();
        Object marker = new Object();
        m.put("k", marker);
        check("  LHM value identity preserved", Boolean.TRUE,
                Boolean.valueOf(m.get("k") == marker));
        check("  LHM remove returns the value", Boolean.TRUE,
                Boolean.valueOf(m.remove("k") == marker));
        m.put("n", null);
        check("  LHM null value stays null", null, m.get("n"));
        check("  LHM containsKey for null value", Boolean.TRUE,
                Boolean.valueOf(m.containsKey("n")));

        // A Set may hold one null element, which has no reference of its own to
        // serve as the PRESENT marker.
        Set<String> withNull = new HashSet<>();
        check("  add(null)", Boolean.TRUE, Boolean.valueOf(withNull.add(null)));
        check("  add(null) again", Boolean.FALSE, Boolean.valueOf(withNull.add(null)));
        check("  contains(null)", Boolean.TRUE, Boolean.valueOf(withNull.contains(null)));
        check("  remove(null)", Boolean.TRUE, Boolean.valueOf(withNull.remove(null)));
        check("  remove(null) again", Boolean.FALSE, Boolean.valueOf(withNull.remove(null)));
        Set<String> lhsNull = new LinkedHashSet<>();
        lhsNull.add(null);
        check("  LHS remove(null)", Boolean.TRUE, Boolean.valueOf(lhsNull.remove(null)));
    }

    /**
     * The map VIEWS — {@code keySet}, {@code entrySet}, {@code values} — are
     * backed by snapshot sets built by {@code map_alloc_node}, whose nodes bind
     * to the real {@code java.util.HashMap$Node} and so DO carry field
     * descriptors. Their PRESENT markers were the ones actually being coerced
     * to null; nothing read them back, which is why it stayed invisible.
     *
     * <p>These gate the mutating view operations that report a boolean, so a
     * marker that stops surviving shows up as a wrong answer, not as silence.
     */
    private static void mapViewsReportMutationCorrectly() {
        System.out.println("== map view sets report their mutations");
        Map<String, String> m = threeEntries();
        check("keySet().remove(present)", Boolean.TRUE,
                Boolean.valueOf(m.keySet().remove("a")));
        check("keySet().remove(absent)", Boolean.FALSE,
                Boolean.valueOf(m.keySet().remove("zz")));
        check("  map shrank", Integer.valueOf(2), Integer.valueOf(m.size()));
        check("  key gone from map", null, m.get("a"));

        m = threeEntries();
        check("keySet().removeAll", Boolean.TRUE,
                Boolean.valueOf(m.keySet().removeAll(Arrays.asList("a", "b"))));
        check("  size", Integer.valueOf(1), Integer.valueOf(m.size()));

        m = threeEntries();
        check("keySet().retainAll", Boolean.TRUE,
                Boolean.valueOf(m.keySet().retainAll(Arrays.asList("a"))));
        check("  size", Integer.valueOf(1), Integer.valueOf(m.size()));

        m = threeEntries();
        Map.Entry<String, String> found = null;
        for (Map.Entry<String, String> e : m.entrySet()) {
            if (e.getKey().equals("a")) {
                found = e;
            }
        }
        check("entrySet().remove(entry)", Boolean.TRUE,
                Boolean.valueOf(m.entrySet().remove(found)));
        check("  size", Integer.valueOf(2), Integer.valueOf(m.size()));

        m = threeEntries();
        check("values().remove", Boolean.TRUE, Boolean.valueOf(m.values().remove("1")));
        check("  size", Integer.valueOf(2), Integer.valueOf(m.size()));

        // The same over a LinkedHashMap, whose nodes are the real Entry class.
        Map<String, String> lm = new LinkedHashMap<>();
        lm.put("a", "1");
        lm.put("b", "2");
        check("LHM keySet().remove", Boolean.TRUE, Boolean.valueOf(lm.keySet().remove("a")));
        check("  order", "[b=2]", entryListOf(lm).toString());

        // A view over many entries, so the snapshot spans several buckets.
        Map<String, String> big = new HashMap<>();
        for (int i = 0; i < 200; i++) {
            big.put("k" + i, "v" + i);
        }
        int reported = 0;
        for (int i = 0; i < 200; i++) {
            if (big.keySet().remove("k" + i)) {
                reported++;
            }
        }
        check("200 keySet removes report true", Integer.valueOf(200),
                Integer.valueOf(reported));
        check("  map emptied", Boolean.TRUE, Boolean.valueOf(big.isEmpty()));
    }

    private static Map<String, String> threeEntries() {
        Map<String, String> m = new HashMap<>();
        m.put("a", "1");
        m.put("b", "2");
        m.put("c", "3");
        return m;
    }

    private static void manyEntriesSurviveResize() {
        System.out.println("== 2000 entries across several resizes");
        LinkedHashMap<Integer, Integer> m = new LinkedHashMap<>();
        for (int i = 0; i < 2000; i++) {
            m.put(Integer.valueOf(i), Integer.valueOf(i * 3));
        }
        check("size", Integer.valueOf(2000), Integer.valueOf(m.size()));
        boolean values = true;
        for (int i = 0; i < 2000; i++) {
            Integer v = m.get(Integer.valueOf(i));
            if (v == null || v.intValue() != i * 3) {
                values = false;
                break;
            }
        }
        checkTrue("every value readable", values);
        int seen = 0;
        boolean ordered = true;
        for (Map.Entry<Integer, Integer> e : m.entrySet()) {
            if (e.getKey().intValue() != seen) {
                ordered = false;
                break;
            }
            seen++;
        }
        checkTrue("insertion order preserved across resizes", ordered);
        check("entries iterated", Integer.valueOf(2000), Integer.valueOf(seen));
    }

    public static void main(String[] args) {
        eldestIsTheRealNode();
        eldestSetValueWritesThroughToTheMap();
        eldestEvictionStillWorks();
        reentrantEvictionIsIdempotent();
        insertionOrderAndBasicOps();
        entrySetSetValueWritesThrough();
        accessOrderIsLru();
        setMembershipIsReportedCorrectly();
        mapViewsReportMutationCorrectly();
        serializationRoundTrip();
        manyEntriesSurviveResize();

        System.out.println(failures == 0 ? "PROBE PASS" : "PROBE FAIL (" + failures + ")");
        if (failures != 0) {
            System.exit(1);
        }
    }
}
