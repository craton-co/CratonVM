import java.lang.reflect.Field;
import java.lang.reflect.Array;
import java.util.concurrent.ConcurrentHashMap;

public class ChmTableSizeProbe {
    public static void main(String[] a) throws Exception {
        Field f = ConcurrentHashMap.class.getDeclaredField("table");
        f.setAccessible(true);
        // table size for a sized ctor with ONE entry (so no resize can occur)
        StringBuilder sb = new StringBuilder();
        for (int c = 1; c <= 80; c++) {
            ConcurrentHashMap<String, Integer> m = new ConcurrentHashMap<>(c);
            m.put("k", 1);
            sb.append(c).append("->").append(Array.getLength(f.get(m))).append(' ');
        }
        System.out.println("sizedCtor(c), 1 entry: " + sb);
        // growth of a default map as entries are added
        ConcurrentHashMap<String, Integer> d = new ConcurrentHashMap<>();
        StringBuilder g = new StringBuilder();
        int last = -1;
        for (int i = 1; i <= 200; i++) {
            d.put("k" + i, i);
            int len = Array.getLength(f.get(d));
            if (len != last) { g.append("size").append(i).append("->").append(len).append(' '); last = len; }
        }
        System.out.println("default growth: " + g);
    }
}
