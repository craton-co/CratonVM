import java.lang.reflect.Field;
import java.util.*;

/** L1 §10 item 3 — WHICH route answers `hashMap.entrySet().toArray()`.
 *
 *  `L1MapFieldProbe` narrowed the `java/util/HashMap` retirement blocker to one
 *  observable: armed, every field of the receiver matches HotSpot, the entry
 *  ITERATOR walks all three pairs and `entrySet().size()` is 3, but
 *  `entrySet().toArray()` is length 0 while `keySet().toArray()` is 3.
 *
 *  Two routes can produce that array and they are told apart by what they do
 *  with a TYPED destination:
 *
 *    * real `AbstractCollection.toArray(T[])` loops its own `iterator()` and
 *      `aastore`s each element, so storing a `Map.Entry` into a `String[]`
 *      raises ArrayStoreException — and CANNOT raise it for an empty walk;
 *    * the VM's `native_hs_to_array_typed` allocates from the element count it
 *      derives from the view's backing, so an empty backing answers a
 *      zero-length array and no throw.
 *
 *  So `ASE` means "real bytecode, three elements" and a quiet `String[0]` means
 *  "a native that believes the view is empty". The rest of the rows pin down
 *  whether the view object itself is the JDK's (`this$0` set, identity stable,
 *  the map's own `entrySet` field populated) or one the VM minted.
 *
 *  Prints shapes and outcomes only — never an element's identity.
 */
public final class L1EntrySetRouteProbe {
    static int rows = 0;

    static void row(String tag, Object v) {
        rows++;
        System.out.println(tag + " |" + v + "|");
    }

    static String call(String what, Body b) {
        try {
            return String.valueOf(b.run());
        } catch (Throwable t) {
            return t.getClass().getName();
        }
    }

    interface Body { Object run() throws Throwable; }

    static Object fieldOf(Object o, String n) {
        for (Class<?> k = o.getClass(); k != null; k = k.getSuperclass()) {
            try {
                Field f = k.getDeclaredField(n);
                f.setAccessible(true);
                return f.get(o);
            } catch (NoSuchFieldException ignored) {
            } catch (Throwable t) {
                return "<" + t.getClass().getSimpleName() + ">";
            }
        }
        return "<no-field>";
    }

    static String shapeOf(Object v) {
        if (v == null) { return "null"; }
        if (v.getClass().isArray()) {
            return "[" + java.lang.reflect.Array.getLength(v) + "]"
                    + v.getClass().getComponentType().getName();
        }
        return v.getClass().getName();
    }

    static void family(String tag, Map<String, String> m) {
        Set<Map.Entry<String, String>> es = m.entrySet();
        Set<String> ks = m.keySet();
        Collection<String> vs = m.values();

        row(tag + ".entrySet.cls", es.getClass().getName());
        row(tag + ".keySet.cls", ks.getClass().getName());
        row(tag + ".entrySet.identityStable", m.entrySet() == m.entrySet());
        row(tag + ".keySet.identityStable", m.keySet() == m.keySet());
        row(tag + ".map.entrySetField", shapeOf(fieldOf(m, "entrySet")));
        row(tag + ".map.keySetField", shapeOf(fieldOf(m, "keySet")));
        row(tag + ".entrySet.this$0", shapeOf(fieldOf(es, "this$0")));
        row(tag + ".keySet.this$0", shapeOf(fieldOf(ks, "this$0")));

        row(tag + ".entrySet.size", call("s", () -> es.size()));
        row(tag + ".entrySet.isEmpty", call("e", () -> es.isEmpty()));
        row(tag + ".entrySet.itrCount", call("i", () -> {
            int n = 0;
            for (Iterator<Map.Entry<String, String>> it = es.iterator(); it.hasNext(); ) { it.next(); n++; }
            return n;
        }));
        row(tag + ".entrySet.toArray", call("a", () -> shapeOf(es.toArray())));
        row(tag + ".entrySet.toArrayObj0", call("a", () -> shapeOf(es.toArray(new Object[0]))));
        row(tag + ".entrySet.toArrayEntry0", call("a", () -> shapeOf(es.toArray(new Map.Entry[0]))));
        // THE DISCRIMINATOR: real AbstractCollection.toArray(T[]) aastores into
        // a String[] and must throw; a native that thinks the view is empty
        // hands back a quiet String[0].
        row(tag + ".entrySet.toArrayString0", call("a", () -> shapeOf(es.toArray(new String[0]))));
        row(tag + ".entrySet.toArrayGen", call("a", () -> shapeOf(es.toArray(Object[]::new))));
        row(tag + ".entrySet.streamCount", call("c", () -> es.stream().count()));
        row(tag + ".entrySet.intoArrayList", call("c", () -> new ArrayList<>(es).size()));
        row(tag + ".entrySet.intoHashSet", call("c", () -> new HashSet<>(es).size()));
        row(tag + ".entrySet.forEachCount", call("c", () -> {
            int[] n = { 0 };
            es.forEach(x -> n[0]++);
            return n[0];
        }));
        row(tag + ".entrySet.spliteratorCount", call("c", () -> {
            int[] n = { 0 };
            es.spliterator().forEachRemaining(x -> n[0]++);
            return n[0];
        }));
        row(tag + ".entrySet.containsAllSelf", call("c", () -> es.containsAll(es)));

        row(tag + ".keySet.toArray", call("a", () -> shapeOf(ks.toArray())));
        row(tag + ".keySet.toArrayString0", call("a", () -> shapeOf(ks.toArray(new String[0]))));
        row(tag + ".values.toArray", call("a", () -> shapeOf(vs.toArray())));
        row(tag + ".values.toArrayString0", call("a", () -> shapeOf(vs.toArray(new String[0]))));

        // Order sensitivity: a FRESH map whose entrySet's very first call is
        // toArray(). If the 0 is a re-entrancy break rather than an empty
        // backing, asking in a different order can change the answer.
        Map<String, String> fresh = new HashMap<>();
        fresh.put("a", "1"); fresh.put("b", "2"); fresh.put("c", "3");
        row(tag + ".fresh.toArrayFirst", call("a", () -> shapeOf(fresh.entrySet().toArray())));
        row(tag + ".fresh.thenSize", call("s", () -> fresh.entrySet().size()));
        Map<String, String> fresh2 = new HashMap<>();
        fresh2.put("a", "1"); fresh2.put("b", "2"); fresh2.put("c", "3");
        row(tag + ".fresh2.itrFirst", call("i", () -> {
            int n = 0;
            for (Map.Entry<String, String> e : fresh2.entrySet()) { n++; }
            return n;
        }));
        row(tag + ".fresh2.thenToArray", call("a", () -> shapeOf(fresh2.entrySet().toArray())));
    }

    public static void main(String[] a) {
        Map<String, String> byPut = new HashMap<>();
        byPut.put("a", "1"); byPut.put("b", "2"); byPut.put("c", "3");
        family("A.hashMap", byPut);

        family("B.linkedHashMap", new LinkedHashMap<>(byPut));

        Map<String, String> ht = new Hashtable<>();
        ht.put("a", "1"); ht.put("b", "2"); ht.put("c", "3");
        family("C.hashtable", ht);

        Map<String, String> tm = new TreeMap<>(byPut);
        family("D.treeMap", tm);

        System.out.println("rows " + rows);
        System.out.println("DONE L1EntrySetRouteProbe");
    }

    private L1EntrySetRouteProbe() { }
}
