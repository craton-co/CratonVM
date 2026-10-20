import java.util.HashMap;
import java.util.LinkedHashMap;

public class HmSemanticsProbe {
    static long exercise(HashMap<Integer, Integer> m, String tag) {
        long sum = 0;
        for (int i = 0; i < 50000; i++) m.put(i, i * 3 + 1);
        for (int i = 0; i < 50000; i++) m.put(i, i * 3 + 2);          // overwrite
        Integer old = m.put(7, 99);                                    // returns previous
        sum += (old == null ? -1 : old);
        sum += (m.get(7) == null ? -1 : m.get(7));
        sum += (m.get(123456) == null ? 1000 : -1000);                 // absent key
        m.put(null, 42);                                               // null key
        sum += (m.get(null) == null ? -1 : m.get(null));
        m.put(8, null);                                                // null value
        sum += (m.get(8) == null ? 2000 : -2000);
        for (int i = 0; i < 50000; i++) {
            Integer v = m.get(i);
            if (v != null) sum += v;
        }
        sum += m.size();
        System.out.println(tag + ": " + sum);
        return sum;
    }

    public static void main(String[] args) {
        long a = exercise(new HashMap<>(), "exact");
        long b = exercise(new LinkedHashMap<>(), "lhm-as-hm");
        System.out.println("MATCH=" + (a == b));
    }
}
