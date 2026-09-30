import java.util.*;
/** Collections.unmodifiableList(x).get(i) — indexed access on the wrapper. */
public class UM {
    static void t(String n, java.util.function.Supplier<Object> s) {
        try { System.out.println(n + " -> " + s.get()); }
        catch (Throwable e) { System.out.println(n + " THREW " + e.getClass().getName() + ": " + e.getMessage()); }
    }
    public static void main(String[] a) {
        List<String> al = new ArrayList<>(List.of("a", "b", "c"));
        List<String> um = Collections.unmodifiableList(al);
        t("unmodifiableList.size()      ", () -> um.size());
        t("unmodifiableList.get(0)      ", () -> um.get(0));
        t("unmodifiableList.get(2)      ", () -> um.get(2));
        t("unmodifiableList iterate     ", () -> { StringBuilder b = new StringBuilder(); for (String s : um) b.append(s); return b; });
        t("unmodifiableList.get(9) oob  ", () -> um.get(9));

        List<String> ll = Collections.unmodifiableList(new LinkedList<>(List.of("x", "y")));
        t("unmod(LinkedList).size()     ", () -> ll.size());
        t("unmod(LinkedList).get(0)     ", () -> ll.get(0));

        List<String> nested = Collections.unmodifiableList(Collections.unmodifiableList(al));
        t("unmod(unmod).get(1)          ", () -> nested.get(1));

        List<String> empty = Collections.unmodifiableList(new ArrayList<>());
        t("unmod(empty).get(0)          ", () -> empty.get(0));

        t("List.copyOf(al).get(0)       ", () -> List.copyOf(al).get(0));
        t("unmod(Arrays.asList).get(0)  ", () -> Collections.unmodifiableList(Arrays.asList("p","q")).get(0));
    }
}
