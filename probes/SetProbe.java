import java.util.*;

public class SetProbe {
    static class Plain { }
    static class WithEq {
        final int id;
        WithEq(int id) { this.id = id; }
        @Override public boolean equals(Object o) { return o instanceof WithEq && ((WithEq) o).id == id; }
        @Override public int hashCode() { return id; }
        @Override public String toString() { return "WithEq(" + id + ")"; }
    }

    static <T> void exercise(String label, Set<T> s, List<T> made) {
        for (T t : made) s.add(t);
        System.out.println(label + " sizeAfterAdd=" + s.size());
        int contains = 0;
        for (T t : made) if (s.contains(t)) contains++;
        System.out.println(label + " contains=" + contains + "/" + made.size());
        int removed = 0;
        for (T t : made) if (s.remove(t)) removed++;
        System.out.println(label + " removeReturnedTrue=" + removed + "/" + made.size()
                + " sizeAfterRemove=" + s.size());
    }

    static List<Plain> plains(int n) {
        List<Plain> l = new ArrayList<>();
        for (int i = 0; i < n; i++) l.add(new Plain());
        return l;
    }

    public static void main(String[] args) {
        exercise("LinkedHashSet<Plain>", new LinkedHashSet<Plain>(), plains(5));
        exercise("HashSet<Plain>      ", new HashSet<Plain>(), plains(5));
        exercise("TreeSet-n/a skipped ", new LinkedHashSet<WithEq>(),
                Arrays.asList(new WithEq(1), new WithEq(2), new WithEq(3)));

        // direct: does the backing map behave?
        Map<Plain, Object> m = new LinkedHashMap<>();
        Plain p = new Plain();
        m.put(p, Boolean.TRUE);
        System.out.println("LinkedHashMap put/remove: containsKey=" + m.containsKey(p)
                + " removeReturned=" + m.remove(p) + " sizeAfter=" + m.size());
        Map<Plain, Object> hm = new HashMap<>();
        Plain q = new Plain();
        hm.put(q, Boolean.TRUE);
        System.out.println("HashMap      put/remove: containsKey=" + hm.containsKey(q)
                + " removeReturned=" + hm.remove(q) + " sizeAfter=" + hm.size());

        // class identity of the sets actually produced
        System.out.println("set class = " + new LinkedHashSet<>().getClass().getName()
                + "  map class = " + new LinkedHashMap<>().getClass().getName()
                + "  hashset class = " + new HashSet<>().getClass().getName());
    }
}
