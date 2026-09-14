import java.util.*;

/** Out-of-range get() on the BACKING list directly (no unmodifiable wrapper). */
public class DirectGetRepro {
    static String t(String what, java.util.concurrent.Callable<Object> c) {
        try { return what + "=" + String.valueOf(c.call()); }
        catch (Throwable e) { return what + "=THREW " + e.getClass().getName(); }
    }
    static void probe(String label, List<String> l) {
        System.out.println(label + " [" + l.getClass().getName() + "] size=" + l.size()
                + "  " + t("get(0)", () -> l.get(0))
                + "  " + t("get(9)", () -> l.get(9))
                + "  " + t("get(-1)", () -> l.get(-1)));
    }
    public static void main(String[] a) {
        probe("ArrayList     ", new ArrayList<>(List.of("a","b")));
        probe("LinkedList    ", new LinkedList<>(List.of("a","b")));
        probe("Vector        ", new Vector<>(List.of("a","b")));
        probe("Arrays.asList ", Arrays.asList("a","b"));
        probe("List.of       ", List.of("a","b"));
        probe("singletonList ", Collections.singletonList("a"));
        probe("emptyList     ", Collections.<String>emptyList());
        probe("CopyOnWrite   ", new java.util.concurrent.CopyOnWriteArrayList<>(List.of("a","b")));
        probe("subList       ", new ArrayList<>(List.of("z","a","b")).subList(1,3));
        probe("AbstractSeq   ", new MyList());
        probe("synchronized  ", Collections.synchronizedList(new LinkedList<>(List.of("a","b"))));
    }
    static class MyList extends AbstractSequentialList<String> {
        private final List<String> d = new ArrayList<>(List.of("a","b"));
        public ListIterator<String> listIterator(int i) { return d.listIterator(i); }
        public int size() { return d.size(); }
    }
}
