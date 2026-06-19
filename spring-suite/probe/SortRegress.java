import java.util.*;

// Regression: normal Comparable natural-ordering paths must still work after the
// compare_via_compare_to guard. Also: a TreeMap of non-Comparable keys must throw
// ClassCastException (caught), not NoSuchMethodError.
public class SortRegress {
    public static void main(String[] args) {
        // TreeSet<Integer> natural order
        TreeSet<Integer> ti = new TreeSet<>(Arrays.asList(5, 2, 9, 3, 1));
        System.out.println("TreeSet<Integer>=" + ti);                  // [1, 2, 3, 5, 9]

        // TreeSet<String> natural order
        TreeSet<String> ts = new TreeSet<>(Arrays.asList("pear", "apple", "fig"));
        System.out.println("TreeSet<String>=" + ts);                   // [apple, fig, pear]

        // TreeMap<String,Integer>
        TreeMap<String,Integer> tm = new TreeMap<>();
        tm.put("b", 2); tm.put("a", 1); tm.put("c", 3);
        System.out.println("TreeMap=" + tm);                           // {a=1, b=2, c=3}

        // Arrays.sort(Object[]) of Comparable
        Integer[] arr = {4, 1, 3, 2};
        Arrays.sort(arr);
        System.out.println("Arrays.sort=" + Arrays.toString(arr));     // [1, 2, 3, 4]

        // Arrays.sort of non-Comparable -> ClassCastException
        Object[] bad = { Integer.class, String.class };
        try { Arrays.sort(bad); System.out.println("Arrays.sort(non-comp) NO-THROW (BAD)"); }
        catch (ClassCastException e) { System.out.println("Arrays.sort(non-comp) CCE OK"); }
        catch (Throwable t) { System.out.println("Arrays.sort(non-comp) BAD " + t.getClass().getName()); }

        // TreeMap of non-Comparable keys -> ClassCastException
        try { TreeMap<Object,Integer> m = new TreeMap<>(); m.put(Integer.class,1); m.put(String.class,2);
              System.out.println("TreeMap(non-comp) NO-THROW (BAD)"); }
        catch (ClassCastException e) { System.out.println("TreeMap(non-comp) CCE OK"); }
        catch (Throwable t) { System.out.println("TreeMap(non-comp) BAD " + t.getClass().getName()); }
    }
}
