import java.util.*;

/**
 * Does a native registered on an ABSTRACT interface method intercept a class
 * the APPLICATION wrote?
 *
 * `scripts/jdk-only-interception.py` measured the surface: eleven natives on
 * `java.util.Map` / `java.util.Collection` methods stand in front of the
 * probe's own implementors. Surface is not behaviour, so this asks the
 * question the surface cannot: run the same operations on a hand-written
 * `Map` and `Collection` whose methods answer something a JDK implementation
 * never would, and see whose answer comes back.
 *
 * Every line is printed as `key=value` so the output diffs byte-for-byte
 * against real HotSpot. A divergence here is a native answering for a class it
 * has never seen; agreement means dispatch reaches the application's bytecode
 * and the interception surface is inert.
 */
public class UserImplementorInterceptProbe {

    /** Answers deliberately unlike any JDK Map: every call is traceable. */
    static final class LoudMap implements Map<String, String> {
        int gets, puts, sizes, containsKeys, keySets, valueses, entrySets;

        @Override public int size() { sizes++; return 4242; }
        @Override public boolean isEmpty() { return false; }
        @Override public boolean containsKey(Object key) { containsKeys++; return "yes".equals(key); }
        @Override public boolean containsValue(Object value) { return false; }
        @Override public String get(Object key) { gets++; return "LOUD:" + key; }
        @Override public String put(String key, String value) { puts++; return "PREV:" + key; }
        @Override public String remove(Object key) { return "REMOVED:" + key; }
        @Override public void putAll(Map<? extends String, ? extends String> m) { }
        @Override public void clear() { }
        @Override public Set<String> keySet() { keySets++; return new LinkedHashSet<>(List.of("k-from-user")); }
        @Override public Collection<String> values() { valueses++; return List.of("v-from-user"); }
        @Override public Set<Entry<String, String>> entrySet() {
            entrySets++;
            return new LinkedHashSet<>(List.of(Map.entry("e-key", "e-val")));
        }
    }

    /** Same idea for the Collection surface. */
    static final class LoudCollection implements Collection<String> {
        int sizes, isEmpties, iterators, toArrays;

        @Override public int size() { sizes++; return 777; }
        @Override public boolean isEmpty() { isEmpties++; return false; }
        @Override public boolean contains(Object o) { return true; }
        @Override public Iterator<String> iterator() {
            iterators++;
            return List.of("it-from-user").iterator();
        }
        @Override public Object[] toArray() { toArrays++; return new Object[] {"arr-from-user"}; }
        @Override public <T> T[] toArray(T[] a) { return a; }
        @Override public boolean add(String s) { return true; }
        @Override public boolean remove(Object o) { return true; }
        @Override public boolean containsAll(Collection<?> c) { return true; }
        @Override public boolean addAll(Collection<? extends String> c) { return true; }
        @Override public boolean removeAll(Collection<?> c) { return true; }
        @Override public boolean retainAll(Collection<?> c) { return true; }
        @Override public void clear() { }
    }

    public static void main(String[] args) {
        LoudMap m = new LoudMap();
        // Through the interface type, so the call site is an invokeinterface on
        // java.util.Map — exactly the shape the natives are registered on.
        Map<String, String> mi = m;
        System.out.println("map.get=" + mi.get("a"));
        System.out.println("map.put=" + mi.put("b", "v"));
        System.out.println("map.size=" + mi.size());
        System.out.println("map.containsKey.yes=" + mi.containsKey("yes"));
        System.out.println("map.containsKey.no=" + mi.containsKey("no"));
        System.out.println("map.keySet=" + mi.keySet());
        System.out.println("map.values=" + mi.values());
        System.out.println("map.entrySet=" + mi.entrySet());
        System.out.println("map.counters=" + m.gets + "," + m.puts + "," + m.sizes + ","
                + m.containsKeys + "," + m.keySets + "," + m.valueses + "," + m.entrySets);

        LoudCollection c = new LoudCollection();
        Collection<String> ci = c;
        System.out.println("coll.size=" + ci.size());
        System.out.println("coll.isEmpty=" + ci.isEmpty());
        System.out.println("coll.iterator.next=" + ci.iterator().next());
        System.out.println("coll.toArray=" + Arrays.toString(ci.toArray()));
        System.out.println("coll.counters=" + c.sizes + "," + c.isEmpties + ","
                + c.iterators + "," + c.toArrays);

        // The same operations reached through code that only knows the
        // interface: a JDK method that calls back into the user's Map.
        System.out.println("map.getOrDefault=" + mi.getOrDefault("z", "dflt"));
        StringBuilder sb = new StringBuilder();
        for (Map.Entry<String, String> e : mi.entrySet()) {
            sb.append(e.getKey()).append('=').append(e.getValue()).append(';');
        }
        System.out.println("map.forEachEntry=" + sb);

        // And a JDK Map for contrast: if the natives are answering, THIS is
        // the shape they were written for and it must keep working.
        Map<String, String> jdk = new HashMap<>();
        jdk.put("x", "1");
        System.out.println("jdk.get=" + jdk.get("x") + " size=" + jdk.size()
                + " keys=" + jdk.keySet());
        System.out.println("PROBE-DONE");
    }
}
