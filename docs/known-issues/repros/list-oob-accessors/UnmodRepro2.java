import java.util.*;

public class UnmodRepro2 {
    static String t(String what, java.util.concurrent.Callable<Object> c) {
        try { return what + "=" + String.valueOf(c.call()); }
        catch (Throwable e) { return what + "=THREW " + e.getClass().getName(); }
    }
    static void probe(String label, List<String> backing) {
        List<String> u = Collections.unmodifiableList(backing);
        System.out.println(label + " [" + backing.getClass().getName() + " -> " + u.getClass().getName() + "]");
        System.out.println("   " + t("size", () -> u.size())
                + "  " + t("get(0)", () -> u.get(0))
                + "  " + t("get(1)", () -> u.get(1))
                + "  " + t("getOOB(9)", () -> u.get(9))
                + "  " + t("getNeg(-1)", () -> u.get(-1)));
        System.out.println("   " + t("subList(0,1)", () -> u.subList(0, 1))
                + "  " + t("listIterator(1).next?", () -> { ListIterator<String> li = u.listIterator(1); return li.hasNext() ? li.next() : "<end>"; })
                + "  " + t("indexOf(b)", () -> u.indexOf("b"))
                + "  " + t("toString", () -> u.toString()));
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
    }
    static class MyList extends AbstractSequentialList<String> {
        private final List<String> d = new ArrayList<>(List.of("a","b"));
        public ListIterator<String> listIterator(int i) { return d.listIterator(i); }
        public int size() { return d.size(); }
    }
}
