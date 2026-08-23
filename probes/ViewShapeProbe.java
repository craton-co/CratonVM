import java.util.*;

/** Which carrier and which iterator class does each map view actually use? */
public class ViewShapeProbe {
    static void row(String label, Map<String, String> m) {
        m.put("a", "1");
        m.put("b", "2");
        System.out.printf("SHAPE %-14s keySet=%-34s ksItr=%-38s%n",
                label, m.keySet().getClass().getName(),
                m.keySet().iterator().getClass().getName());
        System.out.printf("SHAPE %-14s entrySet=%-32s esItr=%-38s%n",
                label, m.entrySet().getClass().getName(),
                m.entrySet().iterator().getClass().getName());
        System.out.printf("SHAPE %-14s values=%-34s vItr=%-38s%n",
                label, m.values().getClass().getName(),
                m.values().iterator().getClass().getName());
    }
    public static void main(String[] args) {
        row("HashMap", new HashMap<>());
        row("LinkedHashMap", new LinkedHashMap<>());
        row("TreeMap", new TreeMap<>());
        row("Hashtable", new Hashtable<>());
    }
}
