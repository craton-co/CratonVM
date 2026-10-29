package cratonvm;
import java.util.*;
public class TckCollections {
    public static int sort_integers() {
        ArrayList<Integer> list = new ArrayList<>();
        list.add(3); list.add(1); list.add(2);
        Collections.sort(list);
        return (list.get(0) == 1 && list.get(1) == 2 && list.get(2) == 3) ? 1 : 0;
    }
    public static int unmodifiable_list() {
        List<String> base = new ArrayList<>(); base.add("a");
        List<String> um = Collections.unmodifiableList(base);
        try { um.add("b"); return 0; } catch (UnsupportedOperationException e) { return 1; }
    }
    public static int singleton_list() {
        List<String> l = Collections.singletonList("x");
        return (l.size() == 1 && "x".equals(l.get(0))) ? 1 : 0;
    }
    public static int empty_list() { return Collections.emptyList().size() == 0 ? 1 : 0; }
    public static int empty_map() { return Collections.emptyMap().size() == 0 ? 1 : 0; }
    public static int empty_set() { return Collections.emptySet().size() == 0 ? 1 : 0; }
    public static int frequency() {
        List<String> l = new ArrayList<>(); l.add("a"); l.add("b"); l.add("a");
        return Collections.frequency(l, "a") == 2 ? 1 : 0;
    }
    public static int max_min() {
        List<Integer> l = new ArrayList<>(); l.add(5); l.add(1); l.add(9);
        return (Collections.max(l) == 9 && Collections.min(l) == 1) ? 1 : 0;
    }
    public static int reverse() {
        List<Integer> l = new ArrayList<>(); l.add(1); l.add(2); l.add(3);
        Collections.reverse(l);
        return (l.get(0) == 3 && l.get(2) == 1) ? 1 : 0;
    }
    public static int singleton_map() {
        Map<String,Integer> m = Collections.singletonMap("k", 42);
        return (m.size() == 1 && m.get("k") == 42) ? 1 : 0;
    }
    public static int singleton_wrappers_are_real_and_immutable() {
        List<String> list = Collections.singletonList("v");
        Set<String> set = Collections.singleton("v");
        Map<String,String> map = Collections.singletonMap("k", "v");
        try {
            map.put("other", "value");
            return 0;
        } catch (UnsupportedOperationException expected) {
            return ("java.util.Collections$SingletonList".equals(list.getClass().getName())
                    && "java.util.Collections$SingletonSet".equals(set.getClass().getName())
                    && "java.util.Collections$SingletonMap".equals(map.getClass().getName())) ? 1 : 0;
        }
    }
}
