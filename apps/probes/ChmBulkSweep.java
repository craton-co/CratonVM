import java.util.*;
import java.util.concurrent.ConcurrentHashMap;

/** The `ConcurrentHashMap` surface the other probes never dispatch.
 *
 *  `ChmShadowSweep` and `MapViewsShadowSweep` between them exercise the map
 *  and view basics. This one exists for the registrations that had
 *  `invocations == 0` in every one of those runs -- the bulk/parallel surface
 *  (`reduce*`, `search*`, `forEach*` with a parallelism threshold) and the
 *  `EntrySetView`/`EntryIterator` classes -- because retiring a native that
 *  nothing ever called is a change no instrument in the tree could see.
 *
 *  It matters more here than it usually would. The retirement is a STORE
 *  MIGRATION: this VM's `put` keeps entries in a segmented side structure,
 *  while real CHM bytecode walks the `table` field. A native reader left
 *  standing over a bytecode-populated map does not throw -- it returns an
 *  EMPTY answer, quietly. Every row below is chosen so that the empty answer
 *  and the right answer PRINT DIFFERENTLY.
 *
 *  Parallelism thresholds are `Long.MAX_VALUE` (sequential) on every call. The
 *  parallel path hands work to a ForkJoinPool whose ORDER neither VM promises,
 *  so a probe that raced it would be measuring the scheduler. The dispatch
 *  under test is the same registration either way.
 *
 *  Every collection is printed SORTED for the same reason: CHM iteration order
 *  is not a property to diff two VMs on.
 */
public class ChmBulkSweep {
    static int rows = 0;

    static void p(String tag, Object v) {
        System.out.println(++rows + " " + tag + " |" + v + "|");
    }

    interface Body {
        Object call() throws Throwable;
    }

    static void t(String tag, Body b) {
        Object v;
        try {
            v = b.call();
        } catch (Throwable e) {
            v = e.getClass().getName() + ": " + e.getMessage();
        }
        p(tag, v);
    }

    static final long SEQ = Long.MAX_VALUE;

    static byte[] ser(Object o) throws Exception {
        java.io.ByteArrayOutputStream b = new java.io.ByteArrayOutputStream();
        try (java.io.ObjectOutputStream os = new java.io.ObjectOutputStream(b)) {
            os.writeObject(o);
        }
        return b.toByteArray();
    }

    static Object deser(byte[] b) throws Exception {
        try (java.io.ObjectInputStream is =
                new java.io.ObjectInputStream(new java.io.ByteArrayInputStream(b))) {
            return is.readObject();
        }
    }

    static ConcurrentHashMap<String, Integer> m() {
        ConcurrentHashMap<String, Integer> m = new ConcurrentHashMap<>();
        for (int i = 0; i < 8; i++) {
            m.put("k" + i, i);
        }
        return m;
    }

    static String sortedOf(Collection<?> c) {
        List<String> l = new ArrayList<>();
        for (Object o : c) {
            l.add(String.valueOf(o));
        }
        Collections.sort(l);
        return l.toString();
    }

    public static void main(String[] a) {
        ConcurrentHashMap<String, Integer> m = m();

        // search: a hit, and a miss that must be null rather than empty.
        t("searchKeys hit", () -> m.searchKeys(SEQ, k -> k.equals("k3") ? k : null));
        t("searchKeys miss", () -> m.searchKeys(SEQ, k -> k.equals("zz") ? k : null));
        t("searchValues hit", () -> m.searchValues(SEQ, v -> v == 5 ? v : null));
        t("searchValues miss", () -> m.searchValues(SEQ, v -> v == 99 ? v : null));
        t("searchEntries hit", () -> m.searchEntries(SEQ, e -> e.getKey().equals("k7") ? e.getValue() : null));
        t("search hit", () -> m.search(SEQ, (k, v) -> v == 2 ? k : null));

        // reduce: an empty store reduces to null or to the identity, so the
        // value distinguishes a real walk from a walk over nothing.
        t("reduceKeysToInt len", () -> m.reduceKeysToInt(SEQ, String::length, 0, Integer::sum));
        t("reduceKeysToLong", () -> m.reduceKeysToLong(SEQ, k -> (long) k.length(), 0L, Long::sum));
        t("reduceKeysToDouble", () -> m.reduceKeysToDouble(SEQ, k -> (double) k.length(), 0.0, Double::sum));
        t("reduceValuesToInt", () -> m.reduceValuesToInt(SEQ, v -> v, 0, Integer::sum));
        t("reduceValuesToLong", () -> m.reduceValuesToLong(SEQ, v -> (long) v, 0L, Long::sum));
        t("reduceValuesToDouble", () -> m.reduceValuesToDouble(SEQ, v -> (double) v, 0.0, Double::sum));
        t("reduceEntriesToInt", () -> m.reduceEntriesToInt(SEQ, e -> e.getValue(), 0, Integer::sum));
        t("reduceEntriesToLong", () -> m.reduceEntriesToLong(SEQ, e -> (long) e.getValue(), 0L, Long::sum));
        t("reduceEntriesToDouble", () -> m.reduceEntriesToDouble(SEQ, e -> (double) e.getValue(), 0.0, Double::sum));
        t("reduceToInt", () -> m.reduceToInt(SEQ, (k, v) -> v, 0, Integer::sum));
        t("reduceToLong", () -> m.reduceToLong(SEQ, (k, v) -> (long) v, 0L, Long::sum));
        t("reduceToDouble", () -> m.reduceToDouble(SEQ, (k, v) -> (double) v, 0.0, Double::sum));
        t("reduceValues max", () -> m.reduceValues(SEQ, (x, y) -> x > y ? x : y));
        t("reduceKeys max", () -> m.reduceKeys(SEQ, (x, y) -> x.compareTo(y) >= 0 ? x : y));
        t("reduceKeys xform", () -> m.reduceKeys(SEQ, k -> k.toUpperCase(), (x, y) -> x.compareTo(y) >= 0 ? x : y));
        t("reduceValues xform", () -> m.reduceValues(SEQ, v -> v * 2, (x, y) -> x > y ? x : y));
        t("reduceEntries xform", () -> m.reduceEntries(SEQ, e -> e.getKey(), (x, y) -> x.compareTo(y) >= 0 ? x : y));
        t("reduce pair", () -> m.reduce(SEQ, (k, v) -> k + "=" + v, (x, y) -> x.compareTo(y) >= 0 ? x : y));

        // forEach with a parallelism threshold, both arities.
        t("forEachKey", () -> {
            List<String> l = new ArrayList<>();
            m.forEachKey(SEQ, l::add);
            Collections.sort(l);
            return l;
        });
        t("forEachKey xform", () -> {
            List<String> l = new ArrayList<>();
            m.forEachKey(SEQ, k -> k.toUpperCase(), l::add);
            Collections.sort(l);
            return l;
        });
        t("forEachValue xform", () -> {
            List<Integer> l = new ArrayList<>();
            m.forEachValue(SEQ, v -> v * 10, l::add);
            Collections.sort(l);
            return l;
        });
        t("forEachEntry", () -> {
            List<String> l = new ArrayList<>();
            m.forEachEntry(SEQ, e -> l.add(e.getKey()));
            Collections.sort(l);
            return l;
        });
        t("forEachEntry xform", () -> {
            List<String> l = new ArrayList<>();
            m.forEachEntry(SEQ, e -> e.getKey() + "!", l::add);
            Collections.sort(l);
            return l;
        });
        t("forEach pair", () -> {
            List<String> l = new ArrayList<>();
            m.forEach(SEQ, (k, v) -> k + ":" + v, l::add);
            Collections.sort(l);
            return l;
        });

        // entrySet view and its iterator: the two classes with the most
        // undispatched registrations.
        Set<Map.Entry<String, Integer>> es = m.entrySet();
        p("entrySet size", es.size());
        p("entrySet isEmpty", es.isEmpty());
        p("entrySet sorted", sortedOf(es));
        t("entrySet contains k4", () -> {
            for (Map.Entry<String, Integer> e : es) {
                if (e.getKey().equals("k4")) {
                    return true;
                }
            }
            return false;
        });
        t("EntryIterator walk", () -> {
            List<String> l = new ArrayList<>();
            Iterator<Map.Entry<String, Integer>> it = es.iterator();
            while (it.hasNext()) {
                Map.Entry<String, Integer> e = it.next();
                l.add(e.getKey() + "=" + e.getValue());
            }
            Collections.sort(l);
            return l;
        });
        t("EntryIterator remove", () -> {
            ConcurrentHashMap<String, Integer> n = m();
            Iterator<Map.Entry<String, Integer>> it = n.entrySet().iterator();
            while (it.hasNext()) {
                if (it.next().getValue() % 2 == 0) {
                    it.remove();
                }
            }
            return n.size() + " " + sortedOf(n.keySet());
        });
        t("entrySet setValue", () -> {
            ConcurrentHashMap<String, Integer> n = m();
            for (Map.Entry<String, Integer> e : n.entrySet()) {
                if (e.getKey().equals("k1")) {
                    e.setValue(111);
                }
            }
            return n.get("k1");
        });
        t("entrySet removeIf", () -> {
            ConcurrentHashMap<String, Integer> n = m();
            n.entrySet().removeIf(e -> e.getValue() < 4);
            return n.size() + " " + sortedOf(n.keySet());
        });
        t("entrySet toArray len", () -> es.toArray().length);
        t("entrySet stream count", () -> es.stream().count());
        t("entrySet equals fresh", () -> es.equals(m().entrySet()));
        t("entrySet hashCode eq", () -> es.hashCode() == m().entrySet().hashCode());

        // keySet/values views on the same map, for the same reason.
        p("keySet sorted", sortedOf(m.keySet()));
        p("values sorted", sortedOf(m.values()));
        t("keySet retainAll", () -> {
            ConcurrentHashMap<String, Integer> n = m();
            n.keySet().retainAll(Arrays.asList("k0", "k1"));
            return sortedOf(n.keySet());
        });
        t("values removeIf", () -> {
            ConcurrentHashMap<String, Integer> n = m();
            n.values().removeIf(v -> v > 3);
            return sortedOf(n.keySet());
        });
        t("newKeySet add", () -> {
            Set<String> s = ConcurrentHashMap.newKeySet();
            s.add("a");
            s.add("b");
            s.add("a");
            return s.size() + " " + sortedOf(s);
        });
        t("keySet(default) add", () -> {
            ConcurrentHashMap<String, Integer> n = new ConcurrentHashMap<>();
            ConcurrentHashMap.KeySetView<String, Integer> v = n.keySet(7);
            v.add("z");
            return n.get("z");
        });

        // The empty map. Every reducer must say "nothing", which is what a
        // reader over a store that MOVED would also say -- so these rows are
        // only meaningful beside the populated ones above, and they are here to
        // keep a fix that empties everything from looking correct on them.
        ConcurrentHashMap<String, Integer> e = new ConcurrentHashMap<>();
        t("empty reduceValues", () -> e.reduceValues(SEQ, (x, y) -> x + y));
        t("empty reduceKeysToInt", () -> e.reduceKeysToInt(SEQ, String::length, 0, Integer::sum));
        t("empty searchKeys", () -> e.searchKeys(SEQ, k -> k));
        t("empty entrySet", () -> e.entrySet().toString());
        t("empty entrySet iterator", () -> e.entrySet().iterator().hasNext());

        // Java serialization, and it is the one hazard these natives were
        // WRITTEN for. `native_chm_write_object` exists because the real JDK
        // bodies walk and rebuild the `table` field that this VM's segmented
        // layout never populates, so a CHM round-tripped through
        // ObjectOutputStream came back EMPTY. Retiring `writeObject` and
        // `readObject` puts those real bodies back in charge -- which is
        // correct only if real `put` bytecode is what filled `table` in the
        // first place. Nothing else in the probe tree round-trips a CHM.
        t("ser CHM size", () -> {
            ConcurrentHashMap<String, Integer> n = m();
            Object back = deser(ser(n));
            return ((ConcurrentHashMap<?, ?>) back).size();
        });
        t("ser CHM content", () -> sortedOf(((ConcurrentHashMap<?, ?>) deser(ser(m()))).entrySet()));
        t("ser CHM class", () -> deser(ser(m())).getClass().getName());
        t("ser CHM empty", () -> ((ConcurrentHashMap<?, ?>) deser(ser(e))).isEmpty());
        t("ser CHM then put", () -> {
            @SuppressWarnings("unchecked")
            ConcurrentHashMap<String, Integer> back =
                (ConcurrentHashMap<String, Integer>) deser(ser(m()));
            back.put("k9", 9);
            return back.size() + " " + back.get("k9") + " " + back.get("k0");
        });
        t("ser Properties", () -> {
            Properties props = new Properties();
            props.setProperty("a", "1");
            props.setProperty("b", "2");
            Properties back = (Properties) deser(ser(props));
            return back.size() + " " + back.getProperty("a") + " " + back.getProperty("b");
        });
        t("ser keySetView", () -> {
            Set<String> v = ConcurrentHashMap.newKeySet();
            v.add("x");
            v.add("y");
            Object back = deser(ser(v));
            return back.getClass().getName() + " " + sortedOf((Collection<?>) back);
        });

        System.out.println("DONE ChmBulkSweep");
    }
}
