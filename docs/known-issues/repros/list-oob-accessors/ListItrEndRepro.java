import java.util.*;

/** Boundary behaviour of listIterator(size).next() and AbstractSequentialList.get. */
public class ListItrEndRepro {
    static String t(String what, java.util.concurrent.Callable<Object> c) {
        try { return what + "=" + String.valueOf(c.call()); }
        catch (Throwable e) { return what + "=THREW " + e.getClass().getName(); }
    }
    public static void main(String[] a) {
        List<String> al = new ArrayList<>(Arrays.asList("a", "b"));
        System.out.println(t("AL.listItr(2).hasNext", () -> al.listIterator(2).hasNext())
                + "  " + t("AL.listItr(2).next", () -> al.listIterator(2).next())
                + "  " + t("AL.itr-exhausted.next", () -> { Iterator<String> i = al.iterator(); i.next(); i.next(); return i.next(); }));
        List<String> ll = new LinkedList<>(Arrays.asList("a", "b"));
        System.out.println(t("LL.listItr(2).next", () -> ll.listIterator(2).next()));
        Foreign f = new Foreign();
        System.out.println(t("Foreign.get(2)", () -> f.get(2))
                + "  " + t("Foreign.get(9)", () -> f.get(9))
                + "  " + t("Foreign.get(-1)", () -> f.get(-1)));
        List<String> uf = Collections.unmodifiableList(f);
        System.out.println(t("view(Foreign).get(2)", () -> uf.get(2))
                + "  " + t("view(Foreign).get(9)", () -> uf.get(9)));
        List<String> cow = new java.util.concurrent.CopyOnWriteArrayList<>(Arrays.asList("a", "b"));
        System.out.println(t("COW.get(2)", () -> cow.get(2))
                + "  " + t("COW.get(9)", () -> cow.get(9))
                + "  " + t("COW.get(-1)", () -> cow.get(-1))
                + "  " + t("COW.set(9,z)", () -> cow.set(9, "z"))
                + "  " + t("COW.add(9,z)", () -> { cow.add(9, "z"); return "ok"; })
                + "  " + t("COW.remove(9)", () -> cow.remove(9)));
    }
    static class Foreign extends AbstractSequentialList<String> {
        private final List<String> d = new ArrayList<>(Arrays.asList("a", "b"));
        public ListIterator<String> listIterator(int i) { return d.listIterator(i); }
        public int size() { return d.size(); }
    }
}
