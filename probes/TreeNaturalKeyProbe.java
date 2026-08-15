import java.util.Map;
import java.util.Set;
import java.util.TreeMap;
import java.util.TreeSet;
import java.util.concurrent.Callable;

/**
 * `tree_natural_order_key_check` refuses the FIRST key of a natural-order
 * TreeMap/TreeSet unless `implements_comparable` says so — a class-hierarchy
 * walk. The comparison it guards, `natural_compare`, orders Strings and
 * primitive wrappers WITHOUT consulting `Comparable` at all (it unboxes).
 *
 * <p>Those two predicates only agree while every wrapper class in the running
 * image actually declares `java.lang.Comparable`. This probe asks whether they
 * do, in whichever mode it is run — a refusal here is the guard over-throwing
 * on a key the very next line could have ordered.
 */
public class TreeNaturalKeyProbe {

    static void row(String name, Callable<Object> c) {
        String out;
        try {
            out = String.valueOf(c.call());
        } catch (Throwable t) {
            out = "THREW " + t.getClass().getName()
                    + (t.getMessage() == null ? "" : ": " + t.getMessage());
        }
        System.out.println("ROW " + name + " = " + out);
    }

    public static void main(String[] args) {
        row("TreeMap<Integer> first put", () -> {
            Map<Integer, String> m = new TreeMap<>();
            m.put(1, "a");
            return m.toString();
        });
        row("TreeMap<Integer> two puts", () -> {
            Map<Integer, String> m = new TreeMap<>();
            m.put(2, "b");
            m.put(1, "a");
            return m.toString();
        });
        row("TreeMap<Long> first put", () -> {
            Map<Long, String> m = new TreeMap<>();
            m.put(1L, "a");
            return m.toString();
        });
        row("TreeMap<Character> first put", () -> {
            Map<Character, String> m = new TreeMap<>();
            m.put('x', "a");
            return m.toString();
        });
        row("TreeMap<Double> first put", () -> {
            Map<Double, String> m = new TreeMap<>();
            m.put(1.5d, "a");
            return m.toString();
        });
        row("TreeMap<Boolean> first put", () -> {
            Map<Boolean, String> m = new TreeMap<>();
            m.put(Boolean.TRUE, "a");
            return m.toString();
        });
        row("TreeMap<String> first put", () -> {
            Map<String, String> m = new TreeMap<>();
            m.put("k", "a");
            return m.toString();
        });
        row("TreeSet<Integer> first add", () -> {
            Set<Integer> s = new TreeSet<>();
            s.add(7);
            return s.toString();
        });
        row("TreeSet<Integer> sorted", () -> {
            Set<Integer> s = new TreeSet<>();
            s.add(3);
            s.add(1);
            s.add(2);
            return s.toString();
        });
        row("Integer implements Comparable", () -> Comparable.class.isAssignableFrom(Integer.class));
        row("Integer.class.getInterfaces",
                () -> java.util.Arrays.toString(Integer.class.getInterfaces()));
        row("Character.class.getInterfaces",
                () -> java.util.Arrays.toString(Character.class.getInterfaces()));
        row("Boolean.class.getInterfaces",
                () -> java.util.Arrays.toString(Boolean.class.getInterfaces()));
        row("TreeMap<Object> non-comparable CCE", () -> {
            Map<Object, String> m = new TreeMap<>();
            m.put(new Object(), "a");
            return "NO THROW " + m;
        });
        System.out.println("PROBE DONE");
    }
}
